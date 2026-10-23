//! Text MR frontend.
//!
//! Compiles the Mythroad MR text dialect (Lua 5.0 family semantics with the
//! keyword and operator surface documented in `docs/mrp/mr-language-vm.md`)
//! directly into `Prototype` trees for the existing VM. The compiler keeps a
//! flat per-function register file: locals occupy the lowest registers in
//! declaration order and temporaries are allocated above them, mirroring the
//! Lua 5.0 codegen model the VM instruction set implements.

use std::sync::Arc;

use crate::{Error, ResourceLimits, Result};

use super::{Constant, MrProfile, Prototype};

const RK_BASE: usize = 250;
const MAX_REGISTER_COUNT: usize = 250;
const JUMP_BIAS: isize = 131_071;
/// Marks a call whose results flow to the caller's stack top (`CALL` with
/// `C == 0`); used by `return f(...)` so every value propagates.
const MULTI_VALUE: usize = usize::MAX;
/// Placeholder name for compiler-internal locals (loop control slots); the
/// lexer can never produce an empty identifier so lookups skip them.
const HIDDEN_LOCAL: &[u8] = b"";

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    Identifier(Arc<[u8]>),
    Number(f64),
    String(Arc<[u8]>),
    Def,
    If,
    Then,
    Else,
    Elif,
    End,
    Nil,
    Local,
    Return,
    While,
    Do,
    Repeat,
    Until,
    For,
    In,
    Break,
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    Dot,
    Comma,
    Assign,
    Equal,
    NotEqual,
    Concat,
    Plus,
    Minus,
    Star,
    Slash,
    And,
    Or,
    Not,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Separator,
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    offset: usize,
}

#[derive(Clone, Debug)]
enum Statement {
    Function {
        name: Arc<[u8]>,
        parameters: Vec<Arc<[u8]>>,
        body: Vec<Statement>,
    },
    Local {
        names: Vec<Arc<[u8]>>,
        values: Vec<Expression>,
    },
    If {
        condition: Expression,
        body: Vec<Statement>,
        else_body: Vec<Statement>,
    },
    While {
        condition: Expression,
        body: Vec<Statement>,
    },
    Repeat {
        body: Vec<Statement>,
        condition: Expression,
    },
    NumericFor {
        variable: Arc<[u8]>,
        start: Expression,
        limit: Expression,
        step: Option<Expression>,
        body: Vec<Statement>,
    },
    GenericFor {
        variables: Vec<Arc<[u8]>>,
        expressions: Vec<Expression>,
        body: Vec<Statement>,
    },
    Break,
    Return {
        values: Vec<Expression>,
    },
    Assign {
        targets: Vec<Expression>,
        values: Vec<Expression>,
    },
    Expression(Expression),
}

#[derive(Clone, Debug)]
enum Expression {
    Nil,
    Number(f64),
    String(Arc<[u8]>),
    Name(Arc<[u8]>),
    Member(Box<Expression>, Arc<[u8]>),
    Call(Box<Expression>, Vec<Expression>),
    Table(Vec<TableField>),
    Or(Box<Expression>, Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    Negate(Box<Expression>),
    Equal(Box<Expression>, Box<Expression>),
    NotEqual(Box<Expression>, Box<Expression>),
    Less(Box<Expression>, Box<Expression>),
    LessEqual(Box<Expression>, Box<Expression>),
    Greater(Box<Expression>, Box<Expression>),
    GreaterEqual(Box<Expression>, Box<Expression>),
    Concat(Box<Expression>, Box<Expression>),
    Add(Box<Expression>, Box<Expression>),
    Subtract(Box<Expression>, Box<Expression>),
    Multiply(Box<Expression>, Box<Expression>),
    Divide(Box<Expression>, Box<Expression>),
}

#[derive(Clone, Debug)]
enum TableField {
    Named(Arc<[u8]>, Expression),
    Array(Expression),
}

pub(in crate::mr) fn compile(source: &[u8], limits: &ResourceLimits) -> Result<Arc<Prototype>> {
    if source.len() > limits.max_mr_string_len {
        return Err(Error::ResourceLimit(format!(
            "remote MR source length {} exceeds {}",
            source.len(),
            limits.max_mr_string_len
        )));
    }
    let tokens = lex(source, limits.max_mr_items)?;
    let statements = Parser::new(tokens, limits.max_mr_depth).parse()?;
    let prototype = Compiler::new(limits, true, Vec::new())?.finish(statements)?;
    let (prototypes, items) = prototype_budget(&prototype);
    if prototypes > limits.max_mr_prototypes {
        return Err(Error::ResourceLimit(format!(
            "remote MR source has {prototypes} prototypes; limit is {}",
            limits.max_mr_prototypes
        )));
    }
    if items > limits.max_mr_items {
        return Err(Error::ResourceLimit(format!(
            "remote MR source compiles to {items} items; limit is {}",
            limits.max_mr_items
        )));
    }
    Ok(prototype)
}

fn lex(source: &[u8], max_tokens: usize) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let offset = cursor;
        let byte = source[cursor];
        let kind = match byte {
            b' ' | b'\t' | b'\r' => {
                cursor += 1;
                continue;
            }
            b'\n' | b';' => {
                cursor += 1;
                TokenKind::Separator
            }
            b'(' => {
                cursor += 1;
                TokenKind::LeftParen
            }
            b')' => {
                cursor += 1;
                TokenKind::RightParen
            }
            b'{' => {
                cursor += 1;
                TokenKind::LeftBrace
            }
            b'}' => {
                cursor += 1;
                TokenKind::RightBrace
            }
            b',' => {
                cursor += 1;
                TokenKind::Comma
            }
            b'+' => {
                cursor += 1;
                TokenKind::Plus
            }
            b'-' if source.get(cursor + 1) == Some(&b'-') => {
                cursor += 2;
                while cursor < source.len() && source[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }
            b'-' => {
                cursor += 1;
                TokenKind::Minus
            }
            b'*' => {
                cursor += 1;
                TokenKind::Star
            }
            b'/' => {
                cursor += 1;
                TokenKind::Slash
            }
            b'=' if source.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                TokenKind::Equal
            }
            b'=' => {
                cursor += 1;
                TokenKind::Assign
            }
            b'!' if source.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                TokenKind::NotEqual
            }
            b'!' => {
                cursor += 1;
                TokenKind::Not
            }
            b'&' if source.get(cursor + 1) == Some(&b'&') => {
                cursor += 2;
                TokenKind::And
            }
            b'|' if source.get(cursor + 1) == Some(&b'|') => {
                cursor += 2;
                TokenKind::Or
            }
            b'<' if source.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                TokenKind::LessEqual
            }
            b'<' => {
                cursor += 1;
                TokenKind::Less
            }
            b'>' if source.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                TokenKind::GreaterEqual
            }
            b'>' => {
                cursor += 1;
                TokenKind::Greater
            }
            b'.' if source.get(cursor + 1) == Some(&b'.') => {
                cursor += 2;
                TokenKind::Concat
            }
            b'.' => {
                cursor += 1;
                TokenKind::Dot
            }
            b'0'..=b'9' => {
                cursor += 1;
                while source
                    .get(cursor)
                    .is_some_and(|byte| {
                        byte.is_ascii_digit()
                            || (*byte == b'.'
                                && source
                                    .get(cursor + 1)
                                    .is_some_and(|next| next.is_ascii_digit()))
                    })
                {
                    cursor += 1;
                }
                let text = std::str::from_utf8(&source[offset..cursor])
                    .map_err(|_| text_error(offset, "number contains non-UTF-8 bytes"))?;
                TokenKind::Number(
                    text.parse()
                        .map_err(|_| text_error(offset, "invalid numeric literal"))?,
                )
            }
            b'\'' | b'"' => {
                let quote = byte;
                cursor += 1;
                let mut value = Vec::new();
                loop {
                    let Some(byte) = source.get(cursor).copied() else {
                        return Err(text_error(offset, "unterminated string literal"));
                    };
                    cursor += 1;
                    if byte == quote {
                        break;
                    }
                    if byte != b'\\' {
                        value.push(byte);
                        continue;
                    }
                    let Some(escaped) = source.get(cursor).copied() else {
                        return Err(text_error(offset, "unterminated string escape"));
                    };
                    cursor += 1;
                    if escaped.is_ascii_digit() {
                        let mut decimal = u16::from(escaped - b'0');
                        for _ in 0..2 {
                            let Some(next) = source.get(cursor).copied() else {
                                break;
                            };
                            if !next.is_ascii_digit() {
                                break;
                            }
                            cursor += 1;
                            decimal = decimal * 10 + u16::from(next - b'0');
                        }
                        let byte = u8::try_from(decimal).map_err(|_| {
                            text_error(cursor, "decimal string escape exceeds one byte")
                        })?;
                        value.push(byte);
                        continue;
                    }
                    value.push(match escaped {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'\\' => b'\\',
                        b'\'' => b'\'',
                        b'"' => b'"',
                        other => other,
                    });
                }
                TokenKind::String(value.into())
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                cursor += 1;
                while source
                    .get(cursor)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    cursor += 1;
                }
                match &source[offset..cursor] {
                    b"def" => TokenKind::Def,
                    b"if" => TokenKind::If,
                    b"then" => TokenKind::Then,
                    b"else" => TokenKind::Else,
                    b"elif" => TokenKind::Elif,
                    b"end" => TokenKind::End,
                    b"nil" => TokenKind::Nil,
                    b"local" => TokenKind::Local,
                    b"return" => TokenKind::Return,
                    b"while" => TokenKind::While,
                    b"do" => TokenKind::Do,
                    b"repeat" => TokenKind::Repeat,
                    b"until" => TokenKind::Until,
                    b"for" => TokenKind::For,
                    b"in" => TokenKind::In,
                    b"break" => TokenKind::Break,
                    identifier => TokenKind::Identifier(Arc::from(identifier)),
                }
            }
            _ => {
                return Err(text_error(offset, format!("unsupported byte {byte:#04x}")));
            }
        };
        tokens.push(Token { kind, offset });
        if tokens.len() > max_tokens {
            return Err(Error::ResourceLimit(format!(
                "remote MR source has more than {max_tokens} tokens"
            )));
        }
    }
    tokens.push(Token {
        kind: TokenKind::Eof,
        offset: source.len(),
    });
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
    max_depth: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>, max_depth: usize) -> Self {
        Self {
            tokens,
            cursor: 0,
            max_depth,
        }
    }

    fn parse(mut self) -> Result<Vec<Statement>> {
        let statements = self.block(0)?;
        self.expect(TokenKind::Eof)?;
        Ok(statements)
    }

    fn block(&mut self, depth: usize) -> Result<Vec<Statement>> {
        if depth >= self.max_depth {
            return Err(Error::ResourceLimit(format!(
                "remote MR source nesting exceeds {}",
                self.max_depth
            )));
        }
        let mut statements = Vec::new();
        self.separators();
        while !self.at_block_end() {
            statements.push(self.statement(depth)?);
            self.separators();
        }
        Ok(statements)
    }

    fn at_block_end(&self) -> bool {
        matches!(
            self.peek(),
            TokenKind::Eof
                | TokenKind::End
                | TokenKind::Else
                | TokenKind::Elif
                | TokenKind::Until
        )
    }

    fn statement(&mut self, depth: usize) -> Result<Statement> {
        if self.take(&TokenKind::Def) {
            let name = self.identifier()?;
            self.expect(TokenKind::LeftParen)?;
            let mut parameters = Vec::new();
            if !matches!(self.peek(), TokenKind::RightParen) {
                loop {
                    parameters.push(self.identifier()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RightParen)?;
            self.separators();
            let body = self.block(depth + 1)?;
            self.expect(TokenKind::End)?;
            return Ok(Statement::Function {
                name,
                parameters,
                body,
            });
        }
        if self.take(&TokenKind::Local) {
            let mut names = vec![self.identifier()?];
            while self.take(&TokenKind::Comma) {
                names.push(self.identifier()?);
            }
            let values = if self.take(&TokenKind::Assign) {
                self.exprlist()?
            } else {
                Vec::new()
            };
            return Ok(Statement::Local { names, values });
        }
        if self.take(&TokenKind::If) {
            return self.if_chain(depth);
        }
        if self.take(&TokenKind::While) {
            let condition = self.expression()?;
            self.expect(TokenKind::Do)?;
            self.separators();
            let body = self.block(depth + 1)?;
            self.expect(TokenKind::End)?;
            return Ok(Statement::While { condition, body });
        }
        if self.take(&TokenKind::Repeat) {
            let body = self.block(depth + 1)?;
            self.expect(TokenKind::Until)?;
            let condition = self.expression()?;
            return Ok(Statement::Repeat { body, condition });
        }
        if self.take(&TokenKind::For) {
            let first = self.identifier()?;
            if self.take(&TokenKind::Assign) {
                let start = self.expression()?;
                self.expect(TokenKind::Comma)?;
                let limit = self.expression()?;
                let step = if self.take(&TokenKind::Comma) {
                    Some(self.expression()?)
                } else {
                    None
                };
                self.expect(TokenKind::Do)?;
                self.separators();
                let body = self.block(depth + 1)?;
                self.expect(TokenKind::End)?;
                return Ok(Statement::NumericFor {
                    variable: first,
                    start,
                    limit,
                    step,
                    body,
                });
            }
            let mut variables = vec![first];
            while self.take(&TokenKind::Comma) {
                variables.push(self.identifier()?);
            }
            self.expect(TokenKind::In)?;
            let expressions = self.exprlist()?;
            self.expect(TokenKind::Do)?;
            self.separators();
            let body = self.block(depth + 1)?;
            self.expect(TokenKind::End)?;
            return Ok(Statement::GenericFor {
                variables,
                expressions,
                body,
            });
        }
        if self.take(&TokenKind::Break) {
            return Ok(Statement::Break);
        }
        if self.take(&TokenKind::Return) {
            self.separators();
            let values = if self.at_block_end() {
                Vec::new()
            } else {
                self.exprlist()?
            };
            return Ok(Statement::Return { values });
        }
        let mut expressions = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            expressions.push(self.expression()?);
        }
        if self.take(&TokenKind::Assign) {
            for expression in &expressions {
                if !matches!(expression, Expression::Name(_) | Expression::Member(_, _)) {
                    return Err(self.error("assignment target must be a name or table member"));
                }
            }
            let values = self.exprlist()?;
            return Ok(Statement::Assign {
                targets: expressions,
                values,
            });
        }
        if expressions.len() != 1 {
            return Err(self.error("expected '=' after the expression list"));
        }
        let expression = expressions.remove(0);
        if !matches!(expression, Expression::Call(_, _)) {
            return Err(self.error("only function calls may be expression statements"));
        }
        Ok(Statement::Expression(expression))
    }

    /// Parses the remainder of an `if` chain whose `if` token has been
    /// consumed. `elif` branches desugar into a nested `if` inside the else
    /// body; the outermost call consumes the single closing `end`.
    fn if_chain(&mut self, depth: usize) -> Result<Statement> {
        let condition = self.expression()?;
        self.expect(TokenKind::Then)?;
        self.separators();
        let body = self.block(depth + 1)?;
        let else_body = if self.take(&TokenKind::Elif) {
            vec![self.if_chain(depth + 1)?]
        } else {
            let fallback = if self.take(&TokenKind::Else) {
                self.separators();
                self.block(depth + 1)?
            } else {
                Vec::new()
            };
            self.expect(TokenKind::End)?;
            fallback
        };
        Ok(Statement::If {
            condition,
            body,
            else_body,
        })
    }

    fn exprlist(&mut self) -> Result<Vec<Expression>> {
        let mut values = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            values.push(self.expression()?);
        }
        Ok(values)
    }

    fn expression(&mut self) -> Result<Expression> {
        self.or()
    }

    fn or(&mut self) -> Result<Expression> {
        let mut expression = self.and()?;
        while self.take(&TokenKind::Or) {
            expression = Expression::Or(Box::new(expression), Box::new(self.and()?));
        }
        Ok(expression)
    }

    fn and(&mut self) -> Result<Expression> {
        let mut expression = self.comparison()?;
        while self.take(&TokenKind::And) {
            expression = Expression::And(Box::new(expression), Box::new(self.comparison()?));
        }
        Ok(expression)
    }

    fn comparison(&mut self) -> Result<Expression> {
        let left = self.concat()?;
        let expression = if self.take(&TokenKind::Equal) {
            Expression::Equal(Box::new(left), Box::new(self.concat()?))
        } else if self.take(&TokenKind::NotEqual) {
            Expression::NotEqual(Box::new(left), Box::new(self.concat()?))
        } else if self.take(&TokenKind::Less) {
            Expression::Less(Box::new(left), Box::new(self.concat()?))
        } else if self.take(&TokenKind::LessEqual) {
            Expression::LessEqual(Box::new(left), Box::new(self.concat()?))
        } else if self.take(&TokenKind::Greater) {
            Expression::Greater(Box::new(left), Box::new(self.concat()?))
        } else if self.take(&TokenKind::GreaterEqual) {
            Expression::GreaterEqual(Box::new(left), Box::new(self.concat()?))
        } else {
            return Ok(left);
        };
        Ok(expression)
    }

    fn concat(&mut self) -> Result<Expression> {
        let left = self.additive()?;
        if self.take(&TokenKind::Concat) {
            return Ok(Expression::Concat(Box::new(left), Box::new(self.concat()?)));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expression> {
        let mut expression = self.multiplicative()?;
        loop {
            if self.take(&TokenKind::Plus) {
                expression =
                    Expression::Add(Box::new(expression), Box::new(self.multiplicative()?));
            } else if self.take(&TokenKind::Minus) {
                expression =
                    Expression::Subtract(Box::new(expression), Box::new(self.multiplicative()?));
            } else {
                return Ok(expression);
            }
        }
    }

    fn multiplicative(&mut self) -> Result<Expression> {
        let mut expression = self.unary()?;
        loop {
            if self.take(&TokenKind::Star) {
                expression =
                    Expression::Multiply(Box::new(expression), Box::new(self.unary()?));
            } else if self.take(&TokenKind::Slash) {
                expression = Expression::Divide(Box::new(expression), Box::new(self.unary()?));
            } else {
                return Ok(expression);
            }
        }
    }

    fn unary(&mut self) -> Result<Expression> {
        if self.take(&TokenKind::Not) {
            return Ok(Expression::Not(Box::new(self.unary()?)));
        }
        if self.take(&TokenKind::Minus) {
            return Ok(Expression::Negate(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expression> {
        let mut expression = self.primary()?;
        loop {
            if self.take(&TokenKind::Dot) {
                expression = Expression::Member(Box::new(expression), self.identifier()?);
                continue;
            }
            if self.take(&TokenKind::LeftParen) {
                let mut arguments = Vec::new();
                if !matches!(self.peek(), TokenKind::RightParen) {
                    loop {
                        arguments.push(self.expression()?);
                        if !self.take(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RightParen)?;
                expression = Expression::Call(Box::new(expression), arguments);
                continue;
            }
            break;
        }
        Ok(expression)
    }

    fn primary(&mut self) -> Result<Expression> {
        let token = self.tokens[self.cursor].clone();
        self.cursor += 1;
        match token.kind {
            TokenKind::Nil => Ok(Expression::Nil),
            TokenKind::Number(number) => Ok(Expression::Number(number)),
            TokenKind::String(bytes) => Ok(Expression::String(bytes)),
            TokenKind::Identifier(name) => Ok(Expression::Name(name)),
            TokenKind::LeftParen => {
                let expression = self.expression()?;
                self.expect(TokenKind::RightParen)?;
                Ok(expression)
            }
            TokenKind::LeftBrace => self.table(),
            _ => Err(text_error(token.offset, "expected expression")),
        }
    }

    fn table(&mut self) -> Result<Expression> {
        let mut fields = Vec::new();
        self.separators();
        while !self.take(&TokenKind::RightBrace) {
            let field = match (
                self.peek(),
                self.tokens.get(self.cursor + 1).map(|token| &token.kind),
            ) {
                (TokenKind::Identifier(_), Some(TokenKind::Assign)) => {
                    let name = self.identifier()?;
                    self.expect(TokenKind::Assign)?;
                    TableField::Named(name, self.expression()?)
                }
                _ => TableField::Array(self.expression()?),
            };
            fields.push(field);
            self.separators();
            if self.take(&TokenKind::Comma) {
                self.separators();
                continue;
            }
            self.expect(TokenKind::RightBrace)?;
            break;
        }
        Ok(Expression::Table(fields))
    }

    fn identifier(&mut self) -> Result<Arc<[u8]>> {
        let token = self.tokens[self.cursor].clone();
        self.cursor += 1;
        match token.kind {
            TokenKind::Identifier(identifier) => Ok(identifier),
            _ => Err(text_error(token.offset, "expected identifier")),
        }
    }

    fn separators(&mut self) {
        while self.take(&TokenKind::Separator) {}
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.cursor].kind
    }

    fn take(&mut self, expected: &TokenKind) -> bool {
        if self.peek() == expected {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: TokenKind) -> Result<()> {
        if self.take(&expected) {
            Ok(())
        } else {
            Err(self.error(format!("expected {expected:?}, got {:?}", self.peek())))
        }
    }

    fn error(&self, message: impl Into<String>) -> Error {
        text_error(self.tokens[self.cursor].offset, message)
    }
}

struct Compiler<'a> {
    limits: &'a ResourceLimits,
    top_level: bool,
    constants: Vec<Constant>,
    prototypes: Vec<Arc<Prototype>>,
    code: Vec<u32>,
    /// Active locals as `(name, register)`; a local's register always equals
    /// its index in this vector, so shadowing resolves to the latest entry
    /// and scope exits truncate the tail.
    locals: Vec<(Arc<[u8]>, usize)>,
    next_register: usize,
    max_register: usize,
    parameter_count: usize,
    /// Pending `break` jumps, one list per enclosing loop.
    breaks: Vec<Vec<usize>>,
}

impl<'a> Compiler<'a> {
    fn new(
        limits: &'a ResourceLimits,
        top_level: bool,
        parameters: Vec<Arc<[u8]>>,
    ) -> Result<Self> {
        if parameters.len() > MAX_REGISTER_COUNT {
            return Err(text_error(0, "too many function parameters"));
        }
        let mut locals = Vec::with_capacity(parameters.len());
        for (index, parameter) in parameters.iter().enumerate() {
            if locals
                .iter()
                .any(|(existing, _)| existing == parameter)
            {
                return Err(text_error(0, "duplicate function parameter"));
            }
            locals.push((parameter.clone(), index));
        }
        let next_register = locals.len();
        Ok(Self {
            limits,
            top_level,
            constants: Vec::new(),
            prototypes: Vec::new(),
            code: Vec::new(),
            locals,
            next_register,
            max_register: next_register,
            parameter_count: parameters.len(),
            breaks: Vec::new(),
        })
    }

    fn finish(mut self, statements: Vec<Statement>) -> Result<Arc<Prototype>> {
        self.block(&statements)?;
        self.emit_abc(27, 0, 1, 0)?;
        let stack_size = self.max_register.max(1);
        Ok(Arc::new(Prototype {
            profile: MrProfile::V50,
            source: Some(Arc::from(&b"@text"[..])),
            line_defined: 0,
            upvalue_count: 0,
            parameter_count: self.parameter_count as u8,
            is_vararg: false,
            max_stack_size: stack_size as u8,
            line_info: vec![0; self.code.len()],
            locals: Vec::new(),
            upvalue_names: Vec::new(),
            constants: self.constants,
            prototypes: self.prototypes,
            code: self.code,
        }))
    }

    /// Compiles a statement sequence as one scope; locals declared inside
    /// stop being visible afterwards and their registers are released.
    fn block(&mut self, statements: &[Statement]) -> Result<()> {
        let mark = self.locals.len();
        for statement in statements {
            self.statement(statement)?;
        }
        self.locals.truncate(mark);
        self.next_register = self.locals.len();
        Ok(())
    }

    fn statement(&mut self, statement: &Statement) -> Result<()> {
        self.next_register = self.locals.len();
        match statement {
            Statement::Function {
                name,
                parameters,
                body,
            } => {
                if !self.top_level {
                    return Err(text_error(0, "nested function definitions are unsupported"));
                }
                let child =
                    Compiler::new(self.limits, false, parameters.clone())?.finish(body.clone())?;
                let prototype = self.prototypes.len();
                self.prototypes.push(child);
                let register = self.allocate(1)?;
                self.emit_abx(34, register, prototype)?;
                self.assign_name(name, register)?;
            }
            Statement::Local { names, values } => {
                let base = self.locals.len();
                // Values are evaluated with the new names still out of scope;
                // the locals only become visible once the whole list settles.
                self.evaluate_list(values, base, names.len())?;
                for (offset, name) in names.iter().enumerate() {
                    let register = base + offset;
                    self.note_register(register)?;
                    self.locals.push((name.clone(), register));
                }
                self.next_register = self.locals.len();
            }
            Statement::If {
                condition,
                body,
                else_body,
            } => {
                let false_jump = self.condition_jump(condition)?;
                self.block(body)?;
                if else_body.is_empty() {
                    self.patch_jump(false_jump, self.code.len())?;
                } else {
                    let end_jump = self.emit_jump()?;
                    self.patch_jump(false_jump, self.code.len())?;
                    self.block(else_body)?;
                    self.patch_jump(end_jump, self.code.len())?;
                }
            }
            Statement::While { condition, body } => {
                let start = self.code.len();
                let false_jump = self.condition_jump(condition)?;
                self.breaks.push(Vec::new());
                self.block(body)?;
                let back = self.emit_jump()?;
                self.patch_jump(back, start)?;
                let end = self.code.len();
                self.patch_jump(false_jump, end)?;
                for jump in self.breaks.pop().expect("loop break list") {
                    self.patch_jump(jump, end)?;
                }
            }
            Statement::Repeat { body, condition } => {
                let start = self.code.len();
                self.breaks.push(Vec::new());
                let mark = self.locals.len();
                for statement in body {
                    self.statement(statement)?;
                }
                // The `until` condition runs in the body's scope, so body
                // locals stay visible while it is evaluated.
                self.next_register = self.locals.len();
                let false_jump = self.condition_jump(condition)?;
                self.locals.truncate(mark);
                self.patch_jump(false_jump, start)?;
                let end = self.code.len();
                for jump in self.breaks.pop().expect("loop break list") {
                    self.patch_jump(jump, end)?;
                }
            }
            Statement::NumericFor {
                variable,
                start,
                limit,
                step,
                body,
            } => {
                let control = self.allocate(3)?;
                self.expression_into(start, control)?;
                self.expression_into(limit, control + 1)?;
                match step {
                    Some(expression) => self.expression_into(expression, control + 2)?,
                    None => {
                        let constant = self.constant(Constant::Number(1.0))?;
                        self.emit_abx(1, control + 2, constant)?;
                    }
                }
                // The VM only provides FORLOOP (increment, then test), so the
                // first iteration is prepared by subtracting the step once.
                self.emit_abc(13, control, control, control + 2)?;
                let loop_jump = self.emit_jump()?;
                let body_start = self.code.len();
                let mark = self.locals.len();
                self.locals.push((variable.clone(), control));
                self.locals
                    .push((Arc::from(HIDDEN_LOCAL), control + 1));
                self.locals
                    .push((Arc::from(HIDDEN_LOCAL), control + 2));
                self.breaks.push(Vec::new());
                for statement in body {
                    self.statement(statement)?;
                }
                let pending = self.breaks.pop().expect("loop break list");
                self.locals.truncate(mark);
                let loop_index = self.code.len();
                self.emit_abc(28, control, 0, 0)?;
                self.patch_jump(loop_jump, loop_index)?;
                self.patch_jump(loop_index, body_start)?;
                let end = self.code.len();
                for jump in pending {
                    self.patch_jump(jump, end)?;
                }
            }
            Statement::GenericFor {
                variables,
                expressions,
                body,
            } => {
                let count = variables.len();
                let base = self.allocate(count + 2)?;
                // Iterator, state and control slots; TFORPREP swaps a bare
                // table for the `next` iterator at run time.
                self.evaluate_list(expressions, base, 3)?;
                let mark = self.locals.len();
                self.locals.push((Arc::from(HIDDEN_LOCAL), base));
                self.locals
                    .push((Arc::from(HIDDEN_LOCAL), base + 1));
                for (offset, variable) in variables.iter().enumerate() {
                    self.locals.push((variable.clone(), base + 2 + offset));
                }
                let prep_index = self.code.len();
                self.emit_abc(30, base, 0, 0)?;
                let body_start = self.code.len();
                self.breaks.push(Vec::new());
                for statement in body {
                    self.statement(statement)?;
                }
                let pending = self.breaks.pop().expect("loop break list");
                self.locals.truncate(mark);
                let loop_index = self.code.len();
                self.emit_abc(29, base, 0, count - 1)?;
                let back = self.emit_jump()?;
                self.patch_jump(back, body_start)?;
                self.patch_jump(prep_index, loop_index)?;
                let end = self.code.len();
                for jump in pending {
                    self.patch_jump(jump, end)?;
                }
            }
            Statement::Break => {
                if self.breaks.is_empty() {
                    return Err(text_error(0, "break outside a loop"));
                }
                let jump = self.emit_jump()?;
                self.breaks
                    .last_mut()
                    .expect("loop break list")
                    .push(jump);
            }
            Statement::Return { values } => {
                if values.is_empty() {
                    self.emit_abc(27, 0, 1, 0)?;
                } else if let [Expression::Call(function, arguments)] = values.as_slice() {
                    // `return f(...)` forwards every result: the call stays
                    // multiret and RETURN reads through the frame's top.
                    let base = self.next_register;
                    self.call_in_place(function, arguments, base, MULTI_VALUE)?;
                    self.emit_abc(27, base, 0, 0)?;
                } else {
                    let base = self.next_register;
                    for (index, expression) in values.iter().enumerate() {
                        let destination = base + index;
                        self.next_register = destination + 1;
                        match expression {
                            Expression::Call(function, arguments) => {
                                self.call_in_place(function, arguments, destination, 1)?
                            }
                            other => self.expression_into(other, destination)?,
                        }
                    }
                    self.emit_abc(27, base, values.len() + 1, 0)?;
                }
            }
            Statement::Assign { targets, values } => {
                let base = self.next_register;
                self.evaluate_list(values, base, targets.len())?;
                for (index, target) in targets.iter().enumerate() {
                    match target {
                        Expression::Name(name) => self.assign_name(name, base + index)?,
                        Expression::Member(object, key) => {
                            let object_register = self.allocate(1)?;
                            self.expression_into(object, object_register)?;
                            let key = self.rk_bytes(key)?;
                            self.emit_abc(9, object_register, key, base + index)?;
                        }
                        _ => unreachable!("parser validates assignment targets"),
                    }
                }
            }
            Statement::Expression(expression) => {
                let Expression::Call(function, arguments) = expression else {
                    unreachable!("parser validates expression statements");
                };
                let destination = self.allocate(1)?;
                self.call_in_place(function, arguments, destination, 0)?;
            }
        }
        self.next_register = self.locals.len();
        Ok(())
    }

    /// Emits `condition_jump`: a conditional skip of the next instruction
    /// followed by a pending `JMP` that lands wherever the caller patches it
    /// (the "condition is false" exit).
    fn condition_jump(&mut self, condition: &Expression) -> Result<usize> {
        match condition {
            Expression::Equal(left, right) => self.compare_jump(21, 0, left, right),
            Expression::NotEqual(left, right) => self.compare_jump(21, 1, left, right),
            Expression::Less(left, right) => self.compare_jump(22, 0, left, right),
            Expression::Greater(left, right) => self.compare_jump(22, 0, right, left),
            Expression::LessEqual(left, right) => self.compare_jump(23, 0, left, right),
            Expression::GreaterEqual(left, right) => self.compare_jump(23, 0, right, left),
            Expression::Not(inner) => {
                let register = self.allocate(1)?;
                self.expression_into(inner, register)?;
                self.emit_abc(24, register, register, 1)?;
                self.emit_jump()
            }
            other => {
                let register = self.allocate(1)?;
                self.expression_into(other, register)?;
                self.emit_abc(24, register, register, 0)?;
                self.emit_jump()
            }
        }
    }

    fn compare_jump(
        &mut self,
        opcode: u8,
        invert: usize,
        left: &Expression,
        right: &Expression,
    ) -> Result<usize> {
        let registers = self.allocate(2)?;
        self.expression_into(left, registers)?;
        self.expression_into(right, registers + 1)?;
        self.emit_abc(opcode, invert, registers, registers + 1)?;
        self.emit_jump()
    }

    /// Evaluates an expression list into consecutive registers starting at
    /// `base`, producing `wanted` slots: when the last expression is a call
    /// and fewer expressions than slots exist, the call expands to fill the
    /// remainder (missing results are nil-filled by the VM); otherwise the
    /// tail is nilled with `LOADNIL`.
    fn evaluate_list(
        &mut self,
        expressions: &[Expression],
        base: usize,
        wanted: usize,
    ) -> Result<()> {
        for (index, expression) in expressions.iter().enumerate() {
            let destination = base + index;
            self.next_register = destination + 1;
            let is_last = index + 1 == expressions.len();
            match expression {
                Expression::Call(function, arguments) => {
                    let results = if is_last && expressions.len() < wanted {
                        wanted - expressions.len() + 1
                    } else {
                        1
                    };
                    self.call_in_place(function, arguments, destination, results)?;
                }
                other => self.expression_into(other, destination)?,
            }
        }
        if wanted > expressions.len() {
            let last = base + wanted - 1;
            self.note_register(last)?;
            if !matches!(expressions.last(), Some(Expression::Call(_, _))) {
                let first = base + expressions.len();
                self.emit_abc(3, first, last, 0)?;
            }
        }
        let span = wanted.max(expressions.len());
        self.next_register = base + span;
        Ok(())
    }

    fn expression_into(&mut self, expression: &Expression, destination: usize) -> Result<()> {
        self.note_register(destination)?;
        match expression {
            Expression::Nil => {
                let constant = self.constant(Constant::Nil)?;
                self.emit_abx(1, destination, constant)?;
            }
            Expression::Number(number) => {
                let constant = self.constant(Constant::Number(*number))?;
                self.emit_abx(1, destination, constant)?;
            }
            Expression::String(bytes) => {
                let constant = self.constant(Constant::Bytes(bytes.clone()))?;
                self.emit_abx(1, destination, constant)?;
            }
            Expression::Name(name) => {
                if let Some(register) = self.lookup_local(name) {
                    self.emit_abc(0, destination, register, 0)?;
                } else {
                    let constant = self.constant(Constant::Bytes(name.clone()))?;
                    self.emit_abx(5, destination, constant)?;
                }
            }
            Expression::Member(object, key) => {
                let object_register = self.allocate(1)?;
                self.expression_into(object, object_register)?;
                let key = self.rk_bytes(key)?;
                self.emit_abc(6, destination, object_register, key)?;
            }
            Expression::Call(function, arguments) => {
                self.call_in_place(function, arguments, destination, 1)?;
            }
            Expression::Table(fields) => {
                self.emit_abc(10, destination, 0, 0)?;
                let mut array_index = 0;
                for field in fields {
                    let checkpoint = self.next_register;
                    let value_register = self.allocate(1)?;
                    let (key, value) = match field {
                        TableField::Named(key, value) => (self.rk_bytes(key)?, value),
                        TableField::Array(value) => {
                            array_index += 1;
                            (self.rk_number(f64::from(array_index))?, value)
                        }
                    };
                    self.expression_into(value, value_register)?;
                    self.emit_abc(9, destination, key, value_register)?;
                    self.next_register = checkpoint;
                }
            }
            Expression::Concat(left, right) => {
                let registers = self.allocate(2)?;
                self.expression_into(left, registers)?;
                self.expression_into(right, registers + 1)?;
                self.emit_abc(19, destination, registers, registers + 1)?;
            }
            Expression::Add(left, right) => self.arithmetic(12, left, right, destination)?,
            Expression::Subtract(left, right) => self.arithmetic(13, left, right, destination)?,
            Expression::Multiply(left, right) => self.arithmetic(14, left, right, destination)?,
            Expression::Divide(left, right) => self.arithmetic(15, left, right, destination)?,
            Expression::Negate(inner) => {
                let source = self.allocate(1)?;
                self.expression_into(inner, source)?;
                self.emit_abc(17, destination, source, 0)?;
            }
            Expression::Not(inner) => {
                let source = self.allocate(1)?;
                self.expression_into(inner, source)?;
                self.emit_abc(18, destination, source, 0)?;
            }
            Expression::Or(left, right) => {
                self.expression_into(left, destination)?;
                self.emit_abc(24, destination, destination, 1)?;
                let jump = self.emit_jump()?;
                self.expression_into(right, destination)?;
                self.patch_jump(jump, self.code.len())?;
            }
            Expression::And(left, right) => {
                self.expression_into(left, destination)?;
                self.emit_abc(24, destination, destination, 0)?;
                let jump = self.emit_jump()?;
                self.expression_into(right, destination)?;
                self.patch_jump(jump, self.code.len())?;
            }
            Expression::Equal(left, right) => {
                self.comparison_value(21, 0, left, right, destination)?
            }
            Expression::NotEqual(left, right) => {
                self.comparison_value(21, 1, left, right, destination)?
            }
            Expression::Less(left, right) => {
                self.comparison_value(22, 0, left, right, destination)?
            }
            Expression::Greater(left, right) => {
                self.comparison_value(22, 0, right, left, destination)?
            }
            Expression::LessEqual(left, right) => {
                self.comparison_value(23, 0, left, right, destination)?
            }
            Expression::GreaterEqual(left, right) => {
                self.comparison_value(23, 0, right, left, destination)?
            }
        }
        Ok(())
    }

    fn arithmetic(
        &mut self,
        opcode: u8,
        left: &Expression,
        right: &Expression,
        destination: usize,
    ) -> Result<()> {
        let left_register = self.allocate(1)?;
        self.expression_into(left, left_register)?;
        let right_operand = self.expression_rk(right)?;
        self.emit_abc(opcode, destination, left_register, right_operand)
    }

    /// Compiles a comparison into a boolean value at `destination`:
    /// `CMP` conditionally skips the following `LOADBOOL false` whose own
    /// skip flag jumps over `LOADBOOL true`.
    fn comparison_value(
        &mut self,
        opcode: u8,
        invert: usize,
        left: &Expression,
        right: &Expression,
        destination: usize,
    ) -> Result<()> {
        let left_register = self.allocate(1)?;
        self.expression_into(left, left_register)?;
        let right_operand = self.expression_rk(right)?;
        self.emit_abc(opcode, invert, left_register, right_operand)?;
        self.emit_abc(2, destination, 0, 1)?;
        self.emit_abc(2, destination, 1, 0)
    }

    /// Returns an RK operand for simple constants, otherwise evaluates the
    /// expression into a fresh register.
    fn expression_rk(&mut self, expression: &Expression) -> Result<usize> {
        match expression {
            Expression::Number(number) => self.rk_number(*number),
            Expression::String(bytes) => self.rk_bytes(bytes),
            other => {
                let register = self.allocate(1)?;
                self.expression_into(other, register)?;
                Ok(register)
            }
        }
    }

    /// Emits a call with the function in `destination` and its arguments in
    /// the registers above it, so `wanted` results land in place:
    /// `0` discards, `1` keeps one, larger values expand across consecutive
    /// registers, `MULTI_VALUE` leaves the caller's top to be read by
    /// `RETURN`.
    fn call_in_place(
        &mut self,
        function: &Expression,
        arguments: &[Expression],
        destination: usize,
        results: usize,
    ) -> Result<()> {
        self.note_register(destination)?;
        self.next_register = destination + 1;
        self.expression_into(function, destination)?;
        for (index, argument) in arguments.iter().enumerate() {
            let register = destination + 1 + index;
            self.next_register = register + 1;
            match argument {
                Expression::Call(function, arguments) => {
                    self.call_in_place(function, arguments, register, 1)?
                }
                other => self.expression_into(other, register)?,
            }
        }
        let c = if results == MULTI_VALUE { 0 } else { results + 1 };
        self.emit_abc(25, destination, arguments.len() + 1, c)?;
        if results != MULTI_VALUE {
            self.next_register = destination + 1 + arguments.len();
        }
        Ok(())
    }

    fn assign_name(&mut self, name: &[u8], register: usize) -> Result<()> {
        if let Some(destination) = self.lookup_local(name) {
            self.emit_abc(0, destination, register, 0)
        } else {
            let constant = self.constant(Constant::Bytes(Arc::from(name)))?;
            self.emit_abx(7, register, constant)
        }
    }

    fn lookup_local(&self, name: &[u8]) -> Option<usize> {
        self.locals
            .iter()
            .rev()
            .find(|(existing, _)| existing.as_ref() == name)
            .map(|(_, register)| *register)
    }

    fn allocate(&mut self, count: usize) -> Result<usize> {
        let start = self.next_register;
        let end = start
            .checked_add(count)
            .ok_or_else(|| text_error(0, "register allocation overflow"))?;
        if end > MAX_REGISTER_COUNT {
            return Err(text_error(0, "remote MR source needs too many registers"));
        }
        self.next_register = end;
        self.max_register = self.max_register.max(end);
        Ok(start)
    }

    fn note_register(&mut self, register: usize) -> Result<()> {
        if register >= MAX_REGISTER_COUNT {
            return Err(text_error(0, "remote MR register is out of range"));
        }
        self.max_register = self.max_register.max(register + 1);
        Ok(())
    }

    fn constant(&mut self, constant: Constant) -> Result<usize> {
        if let Some(index) = self
            .constants
            .iter()
            .position(|existing| constants_equal(existing, &constant))
        {
            return Ok(index);
        }
        if self.constants.len() >= 262_144 {
            return Err(text_error(0, "remote MR source has too many constants"));
        }
        let index = self.constants.len();
        self.constants.push(constant);
        Ok(index)
    }

    fn rk_bytes(&mut self, bytes: &[u8]) -> Result<usize> {
        let constant = self.constant(Constant::Bytes(Arc::from(bytes)))?;
        let operand = RK_BASE + constant;
        if operand >= 512 {
            return Err(text_error(
                0,
                "remote MR source has too many constants for a table key",
            ));
        }
        Ok(operand)
    }

    fn rk_number(&mut self, number: f64) -> Result<usize> {
        let constant = self.constant(Constant::Number(number))?;
        let operand = RK_BASE + constant;
        if operand >= 512 {
            return Err(text_error(
                0,
                "remote MR source has too many constants for a table key",
            ));
        }
        Ok(operand)
    }

    fn emit_abc(&mut self, opcode: u8, a: usize, b: usize, c: usize) -> Result<()> {
        if a >= 256 || b >= 512 || c >= 512 {
            return Err(text_error(0, "MR instruction operand is out of range"));
        }
        self.code
            .push(u32::from(opcode) | ((c as u32) << 6) | ((b as u32) << 15) | ((a as u32) << 24));
        Ok(())
    }

    fn emit_abx(&mut self, opcode: u8, a: usize, bx: usize) -> Result<()> {
        if a >= 256 || bx >= 262_144 {
            return Err(text_error(0, "MR instruction operand is out of range"));
        }
        self.code
            .push(u32::from(opcode) | ((bx as u32) << 6) | ((a as u32) << 24));
        Ok(())
    }

    fn emit_jump(&mut self) -> Result<usize> {
        let index = self.code.len();
        self.emit_abx(20, 0, JUMP_BIAS as usize)?;
        Ok(index)
    }

    /// Rewrites the sBx field of the instruction at `index`, preserving its
    /// opcode and A operand so `JMP`, `FORLOOP` and `TFORPREP` share one
    /// patching path.
    fn patch_jump(&mut self, index: usize, target: usize) -> Result<()> {
        let offset = target as isize - index as isize - 1;
        let encoded = JUMP_BIAS
            .checked_add(offset)
            .filter(|encoded| (0..262_144).contains(encoded))
            .ok_or_else(|| text_error(0, "remote MR jump is out of range"))?;
        let instruction = self.code[index];
        let opcode = instruction & 0x3f;
        let a = instruction & 0xff00_0000;
        self.code[index] = opcode | a | ((encoded as u32) << 6);
        Ok(())
    }
}

fn constants_equal(left: &Constant, right: &Constant) -> bool {
    match (left, right) {
        (Constant::Nil, Constant::Nil) => true,
        (Constant::Number(left), Constant::Number(right)) => left.to_bits() == right.to_bits(),
        (Constant::Bytes(left), Constant::Bytes(right)) => left == right,
        _ => false,
    }
}

fn prototype_budget(prototype: &Prototype) -> (usize, usize) {
    let mut prototypes: usize = 1;
    let mut items = prototype
        .code
        .len()
        .saturating_add(prototype.constants.len());
    for child in &prototype.prototypes {
        let (child_prototypes, child_items) = prototype_budget(child);
        prototypes = prototypes.saturating_add(child_prototypes);
        items = items.saturating_add(child_items);
    }
    (prototypes, items)
}

fn text_error(offset: usize, message: impl Into<String>) -> Error {
    Error::MrFault(format!(
        "remote MR source at byte {offset}: {}",
        message.into()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_the_update_progress_callback() {
        let source = br#"print("code frame received")
def progress(data)
  p = tonumber(data)
  if p == nil then
    p = 0
  end
  if g_dialog then
    if g_dialog.update then
      g_dialog.update(g_dialog, nil, data .. "%", p)
    end
  end
  if win then
    if win.refresh then
      win.refresh()
    end
  end
end
cmd.progress = progress"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        assert_eq!(prototype.prototypes.len(), 1);
        assert!(prototype.code.len() >= 4);
        assert!(prototype.prototypes[0].code.len() >= 20);
    }

    #[test]
    fn compiles_binary_file_update_with_failure_branch() {
        let source = br#"f = file.open("applist.mrp", 10)
if f then
  f.write(f, "\077\082\080\071\000")
  f.close(f)
  if recreateApplist then
    recreateApplist(1)
  else
    net.successexit()
  end
else
  net.fail()
end"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        assert!(prototype.constants.iter().any(
            |constant| matches!(constant, Constant::Bytes(bytes) if bytes.as_ref() == b"MRPG\0")
        ));
        assert!(prototype.code.len() >= 20);
    }

    #[test]
    fn compiles_download_byte_counter_addition() {
        let source = br#"def dl_file(data)
  if dl_f then
    dl_f.write(dl_f, data)
    dl_written = dl_written + string.len(data)
  end
end"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        assert!(
            prototype.prototypes[0]
                .code
                .iter()
                .any(|instruction| instruction & 0x3f == 12)
        );
    }

    #[test]
    fn compiles_nested_application_list_tables() {
        let source = br#"listver = 1
ignore_all = nil
list = {
  {t = "APP", e = "gghjt", v = 1002, ic = 1},
  {t = "APP", e = "talkcat", v = 1040, ic = 2},
}"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        assert!(prototype.code.len() >= 20);
        assert!(prototype.constants.iter().any(
            |constant| matches!(constant, Constant::Bytes(bytes) if bytes.as_ref() == b"talkcat")
        ));
    }

    #[test]
    fn rejects_decimal_string_escapes_larger_than_a_byte() {
        let error = compile(b"print(\"\\256\")", &ResourceLimits::default()).unwrap_err();
        assert!(error.to_string().contains("exceeds one byte"));
    }

    #[test]
    fn rejects_source_outside_the_supported_grammar() {
        let error = compile(b"x + 1", &ResourceLimits::default()).unwrap_err();
        assert!(error.to_string().contains("only function calls"));
    }

    #[test]
    fn compiles_the_aero_text_entry_shape() {
        let source = br#"def dealevent(code, p0, p1, p2)
   local packet
   if _t(p0) == "str" then
      local p00 = p0 .. "\0"
      local p10 = p1 .. "\0"
      local pcontent, content_len = string.subV(p00)
      local pnumber, number_len = string.subV(p10)
      packet = string.pack("iiiii", code, pcontent, pnumber, p2, content_len)
   else
      packet = string.pack("iii", code, p0 || 0, p1 || 0)
   end
   local value, result = _strCom(801, packet, 1)
   return result
end

def dealtimer()
   local value, result = _strCom(801, "", 2), 801
   return result
end

local image = _strCom(601, "cfunction.ext")
if image then
   local result = _strCom(800, image, 0)
   if result == 0 then
      _strCom(801, "", 0)
   else
      Exit()
   end
else
   Exit()
end"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        assert_eq!(prototype.prototypes.len(), 2);
        let dealevent = &prototype.prototypes[0];
        assert_eq!(dealevent.parameter_count, 4);
        // `local value, result = _strCom(...)` expands the call to two slots.
        assert!(dealevent.code.iter().any(|instruction| {
            (instruction & 0x3f == 25) && ((instruction >> 6) & 0x1ff == 3)
        }));
        // `return result` ends the frame with exactly one value.
        assert!(dealevent.code.iter().any(|instruction| {
            (instruction & 0x3f == 27)
                && ((instruction >> 24) & 0xff > 0)
                && (((instruction >> 15) & 0x1ff) == 2)
        }));
    }

    #[test]
    fn multi_value_local_lists_expand_the_trailing_call() {
        let source = br#"local a, b, c = f()
local d, e = g(), 801
local x
local y, z = 1, 2"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        // `f()` must ask for three results (CALL with C == 4) while `g()` is
        // truncated to one (C == 2) and `801` fills the second slot.
        let calls: Vec<u32> = prototype
            .code
            .iter()
            .filter(|instruction| **instruction & 0x3f == 25)
            .map(|instruction| (instruction >> 6) & 0x1ff)
            .collect();
        assert!(calls.contains(&4), "expanding call missing: {calls:?}");
        assert!(calls.contains(&2), "truncated call missing: {calls:?}");
        // Bare `local x` nils its register (LOADNIL opcode 3).
        assert!(prototype.code.iter().any(|instruction| instruction & 0x3f == 3));
    }

    #[test]
    fn compiles_control_flow_and_logic() {
        let source = br#"total = 0
local i = 0
while i < 4 do
  i = i + 1
  if i == 2 then
    continue_missing = 1
  elif i == 3 then
    total = total + 100
  else
    break
  end
end
for k = 1, 10, 2 do
  total = total + k
end
for key, value in items do
  total = total + value
end
repeat
  total = total - 1
until total <= 0
scaled = total * 2 / 4
grade = !flag && a || b
other = x != y"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        let opcodes = |code: &[u32]| -> Vec<u32> {
            code.iter().map(|instruction| instruction & 0x3f).collect()
        };
        let opcodes = opcodes(&prototype.code);
        assert!(opcodes.contains(&28), "FORLOOP missing");
        assert!(opcodes.contains(&29), "TFORLOOP missing");
        assert!(opcodes.contains(&30), "TFORPREP missing");
        assert!(opcodes.contains(&24), "TEST missing");
        assert!(opcodes.contains(&18), "NOT missing");
        assert!(opcodes.contains(&21), "compare missing");
        assert!(opcodes.contains(&13), "subtract missing");
        assert!(opcodes.contains(&14), "multiply missing");
    }

    #[test]
    fn return_forwards_the_trailing_call_as_multivalue() {
        let source = br#"def pipe(x)
  return f(x)
end"#;
        let prototype = compile(source, &ResourceLimits::default()).unwrap();
        let code = &prototype.prototypes[0].code;
        // The call stays multiret (C == 0) and RETURN reads through top.
        let call = code.iter().find(|instruction| **instruction & 0x3f == 25).unwrap();
        assert_eq!((call >> 6) & 0x1ff, 0);
        let ret = code.iter().find(|instruction| **instruction & 0x3f == 27).unwrap();
        assert_eq!((ret >> 15) & 0x1ff, 0);
    }
}
