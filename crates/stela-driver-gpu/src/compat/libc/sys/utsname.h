/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_LIBC_SYS_UTSNAME_H
#define STELA_LIBC_SYS_UTSNAME_H
#include <sys/cdefs.h>
__BEGIN_DECLS
struct utsname {
	char sysname[65];
	char nodename[65];
	char release[65];
	char version[65];
	char machine[65];
	char domainname[65];
};
/* No kernel of its own to name: fails, so callers leave its version out. */
int uname(struct utsname *name);
__END_DECLS
#endif
