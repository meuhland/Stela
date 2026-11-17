/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_LIBC_LINK_H
#define STELA_LIBC_LINK_H
#include <stddef.h>
#include <sys/cdefs.h>
__BEGIN_DECLS
/* The image is one PE file with no ELF objects, so no object's program
 * headers are ever passed and the type stays incomplete. */
struct dl_phdr_info;
int dl_iterate_phdr(int (*callback)(struct dl_phdr_info *info, size_t size, void *data),
		    void *data);
__END_DECLS
#endif
