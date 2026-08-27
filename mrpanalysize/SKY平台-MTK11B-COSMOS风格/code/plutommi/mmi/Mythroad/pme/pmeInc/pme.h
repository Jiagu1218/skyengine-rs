#ifndef __PME_H__
#define __PME_H__


/*功能开关*/
//#define PME_NEWS_POP_ANYWHERE //在任何界面都可以弹出pop提示框
#define __IS_11A__ //MTK soft is 10A or higher.



/*****************************************************************************
 * CONST & STRUCT
 * 
 *****************************************************************************/

/*NVRAM 数据长度(BYTE)*/
#define PME_NV_CONTENT_SIZE		40


/*****************************************************************************
 * FUNCTION: pme_idle_notify
 * DESCRIPTION: 每次回到待机界面调用
 * PARAMETERS:
 * RETURNS: void
 *****************************************************************************/
extern void pme_idle_notify(int flag);

/*****************************************************************************
 * FUNCTION: pme_main_highlight_handler
 * DESCRIPTION: 菜单入口hili函数
 * PARAMETERS:
 * RETURNS: void
 *****************************************************************************/
void pme_main_highlight_handler(void);

void pme_clear_idle_msg_ind(void);
#endif

