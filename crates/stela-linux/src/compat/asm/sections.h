/* SPDX-License-Identifier: GPL-2.0 */
/* Stela's arch: Linux's code is part of a larger image whose read-only data
 * has no pair of section bounds. Whether an address is immutable image data
 * is the platform's answer (stela_linux_is_rodata), which kstrdup_const
 * already takes; is_kernel_rodata gives the same one. */
#ifndef STELA_ASM_SECTIONS_H
#define STELA_ASM_SECTIONS_H
#include <linux/types.h>

bool stela_linux_is_rodata(const void *address);

#define is_kernel_rodata is_between_rodata_bounds
#include <asm-generic/sections.h>
#undef is_kernel_rodata

static inline bool is_kernel_rodata(unsigned long addr)
{
	return stela_linux_is_rodata((const void *)addr);
}
#endif
