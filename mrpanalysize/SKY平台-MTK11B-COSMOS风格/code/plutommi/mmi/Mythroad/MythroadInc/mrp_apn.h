#ifdef __MMI_DSM_NEW__
#ifndef __DSM_APN_H__
#define __DSM_APN_H__


/**
 * \brief APN 数据结构
 */
typedef struct
{
	char* NetId;
	char *apn;
	char *userName;
	char *passWord;
	unsigned char dns[4];
	unsigned char authentication_type;
}T_DSM_APN;


/**
 * \brief 通过PLMN来获取当地APN的数据
 *
 * \param netId	[in] PLMN 串
 * \return 返回查找到的APN数据指针，查不到返回NULL.
 */
T_DSM_APN *mr_getAPN(unsigned char * netId);


#endif
#endif
