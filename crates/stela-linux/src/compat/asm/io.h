/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_STELA_IO_H
#define _ASM_STELA_IO_H
/* asm-generic's MMIO accessors: volatile accesses with x86's fences. Device
 * memory is mapped by Stela's device owners, so the kernel-API's ioremap()
 * maps nothing. Port I/O, where the profile selects it, is x86's. */
#include <linux/types.h>

void __iomem *ioremap(phys_addr_t offset, size_t size);
#define ioremap ioremap
void __iomem *ioremap_wc(phys_addr_t offset, size_t size);
#define ioremap_wc ioremap_wc
void iounmap(volatile void __iomem *addr);
#define iounmap iounmap

#ifdef CONFIG_HAS_IOPORT
/* x86's port I/O instructions. A port number is 16 bits wide. */
#define STELA_PORT_IO(type, bwl) \
	static inline type _in##bwl(unsigned long port) \
	{ \
		type value; \
		asm volatile("in" #bwl " %w1, %0" : "=a"(value) : "Nd"((u16)port)); \
		return value; \
	} \
	static inline void _out##bwl(type value, unsigned long port) \
	{ \
		asm volatile("out" #bwl " %0, %w1" : : "a"(value), "Nd"((u16)port)); \
	} \
	static inline void ins##bwl(unsigned long port, void *buffer, unsigned int count) \
	{ \
		for (type *to = buffer; count--; to++) \
			*to = _in##bwl(port); \
	} \
	static inline void outs##bwl(unsigned long port, const void *buffer, \
				     unsigned int count) \
	{ \
		for (const type *from = buffer; count--; from++) \
			_out##bwl(*from, port); \
	}
STELA_PORT_IO(u8, b)
STELA_PORT_IO(u16, w)
STELA_PORT_IO(u32, l)
#undef STELA_PORT_IO
#define _inb _inb
#define _inw _inw
#define _inl _inl
#define _outb _outb
#define _outw _outw
#define _outl _outl
#define insb insb
#define insw insw
#define insl insl
#define outsb outsb
#define outsw outsw
#define outsl outsl
#endif

#include <asm-generic/io.h>
#endif
