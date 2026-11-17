/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_LINUX_BACKLIGHT_H
#define STELA_LINUX_BACKLIGHT_H
#include <linux/types.h>
/* What boot read from the ACPI companion of the GPU at domain, bus and devfn
 * before it started, which upstream nouveau reads through ACPI. Each returns
 * 0 with its outputs set (alive for the boot), or -ENOENT. */

/* The NBCI or NVHG _DSM function 0x14 backlight table. */
int stela_linux_gpu_backlight_data(u32 domain, u32 bus, u32 devfn,
				   const u8 **data, size_t *size);
/* The _DOD display IDs, up to max of them; -E2BIG when more. */
int stela_linux_gpu_acpi_dod(u32 domain, u32 bus, u32 devfn,
			     u32 *ids, u32 max, u32 *count);
/* A capability word: which 0 is JT _DSM function 1's, 1 NVOP 0x1a's. */
int stela_linux_gpu_acpi_caps(u32 domain, u32 bus, u32 devfn,
			      u32 which, u32 *value);
#endif
