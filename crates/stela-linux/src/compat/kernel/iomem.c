// SPDX-License-Identifier: GPL-2.0-or-later
/* Linux's copies to and from I/O memory, as x86's string copies make them:
 * a byte and a word align the device's side, then 32-bit accesses carry the
 * rest, ending in a word and a byte. A device read is a bus round trip, so a
 * copy a byte at a time costs four of x86's: amdgpu's memory training saves
 * and restores 32 MB of VRAM through the BAR this way. */
#include <linux/io.h>
#include <linux/mm.h>
#include <stela/linux.h>
#include <stela/pci.h>

void memcpy_fromio(void *dst, const volatile void __iomem *src, size_t count)
{
	u8 *to = dst;
	const volatile u8 __iomem *from = src;

	if (count && ((unsigned long)from & 1)) {
		*to++ = __raw_readb(from++);
		count--;
	}
	if (count > 1 && ((unsigned long)from & 2)) {
		u16 word = __raw_readw(from);
		memcpy(to, &word, 2);
		to += 2;
		from += 2;
		count -= 2;
	}
	for (; count >= 4; count -= 4, to += 4, from += 4) {
		u32 dword = __raw_readl(from);
		memcpy(to, &dword, 4);
	}
	if (count >= 2) {
		u16 word = __raw_readw(from);
		memcpy(to, &word, 2);
		to += 2;
		from += 2;
		count -= 2;
	}
	if (count)
		*to = __raw_readb(from);
}

void memcpy_toio(volatile void __iomem *dst, const void *src, size_t count)
{
	volatile u8 __iomem *to = dst;
	const u8 *from = src;

	if (count && ((unsigned long)to & 1)) {
		__raw_writeb(*from++, to++);
		count--;
	}
	if (count > 1 && ((unsigned long)to & 2)) {
		u16 word;
		memcpy(&word, from, 2);
		__raw_writew(word, to);
		to += 2;
		from += 2;
		count -= 2;
	}
	for (; count >= 4; count -= 4, to += 4, from += 4) {
		u32 dword;
		memcpy(&dword, from, 4);
		__raw_writel(dword, to);
	}
	if (count >= 2) {
		u16 word;
		memcpy(&word, from, 2);
		__raw_writew(word, to);
		to += 2;
		from += 2;
		count -= 2;
	}
	if (count)
		__raw_writeb(*from, to);
}

void memset_io(volatile void __iomem *addr, int val, size_t count)
{
	volatile u8 __iomem *to = addr;
	u32 pattern = 0x01010101u * (u8)val;

	while (count && ((unsigned long)to & 3)) {
		__raw_writeb((u8)val, to++);
		count--;
	}
	for (; count >= 4; count -= 4, to += 4)
		__raw_writel(pattern, to);
	while (count--)
		__raw_writeb((u8)val, to++);
}

/* Device memory is mapped by its device's owner, in whole pages. */
static void __iomem *map(phys_addr_t offset, size_t size, bool write_combining)
{
	const struct stela_linux_owner *owner = stela_linux_owner();
	void __iomem *rom = stela_linux_pci_rom_map(offset, size);

	if (rom)
		return rom;

	if (!owner->services->map_device) {
		stela_linux_unsupported("device memory mappings");
		return NULL;
	}
	phys_addr_t base = offset & PAGE_MASK;
	u8 *mapping = owner->services->map_device(owner->context, base,
						  PAGE_ALIGN(offset + size) - base,
						  write_combining);
	return mapping ? (void __iomem *)(mapping + (offset - base)) : NULL;
}

void __iomem *ioremap(phys_addr_t offset, size_t size)
{
	return map(offset, size, false);
}

void __iomem *ioremap_wc(phys_addr_t offset, size_t size)
{
	return map(offset, size, true);
}

/* Uncached mappings serve every memremap flag: they order more strictly than
 * any flag asks. */
void *memremap(resource_size_t offset, size_t size, unsigned long flags)
{
	(void)flags;
	return (void __force *)ioremap(offset, size);
}

void memunmap(void *addr)
{
	iounmap((void __iomem __force *)addr);
}

void iounmap(volatile void __iomem *addr)
{
	const struct stela_linux_owner *owner = stela_linux_owner();

	/* A platform ROM copy stays until its function is released. */
	if (addr && !stela_linux_pci_rom_mapped(addr))
		owner->services->unmap_device(owner->context,
					      (void *)((unsigned long)addr & PAGE_MASK));
}
