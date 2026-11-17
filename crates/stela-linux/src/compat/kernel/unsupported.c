// SPDX-License-Identifier: GPL-2.0-or-later
/* Kernel services Stela does not provide yet. */
#include <linux/folio_batch.h>
#include <linux/fs.h>
#include <linux/hmm.h>
#include <linux/i2c.h>
#include <linux/io.h>
#include <linux/kthread.h>
#include <linux/mm.h>
#include <linux/platform_device.h>
#include <linux/property.h>
#include <linux/rcupdate.h>
#include <linux/regmap.h>
#include <linux/sched/mm.h>
#include <linux/seq_file.h>
#include <linux/swap.h>
#include <stela/linux.h>
#include <unsupported.h>

/* Pages behind shmem files: shmem-backed GEM objects and TTM's backup of
 * buffers it swaps out. TTM allocates pages from its own pool and Stela never
 * reclaims them, so nothing needs them; a page asked for anyway is refused,
 * said once, with the error Linux gives when shmem has no page to give, which
 * each caller returns (a GEM object's pages fail, a buffer stays unswapped).
 * No shmem page is ever read in, so a truncation finds none and a writeout or
 * a dirty bit has no page to act on. */
struct folio *shmem_read_folio_gfp(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	(void)mapping;
	(void)index;
	(void)gfp;
	stela_linux_unsupported("pages of a shmem file: they are refused");
	return ERR_PTR(-ENOMEM);
}

struct page *shmem_read_mapping_page_gfp(struct address_space *mapping, pgoff_t index, gfp_t gfp)
{
	(void)mapping;
	(void)index;
	(void)gfp;
	stela_linux_unsupported("pages of a shmem file: they are refused");
	return ERR_PTR(-ENOMEM);
}

void shmem_truncate_range(struct inode *inode, loff_t start, uoff_t end)
{
	(void)inode;
	(void)start;
	(void)end;
}

struct swap_iocb;
int shmem_writeout(struct folio *folio, struct swap_iocb **plug, struct list_head *folio_list)
{
	(void)folio;
	(void)plug;
	(void)folio_list;
	stela_linux_unsupported("writing a shmem page out: it is refused");
	return -EOPNOTSUPP;
}

bool folio_clear_dirty_for_io(struct folio *folio)
{
	(void)folio;
	stela_linux_unsupported("writing a shmem page out: it is refused");
	return false;
}

/* An address for a client's mapping of a shmem-backed object, which only a
 * client with a Linux address space asks for; Stela's clients map through
 * their owner. It is refused, said once, as Linux refuses an address it
 * cannot place. */
unsigned long mm_get_unmapped_area(struct file *file, unsigned long address, unsigned long length,
				   unsigned long offset, unsigned long flags)
{
	(void)file;
	(void)address;
	(void)length;
	(void)offset;
	(void)flags;
	stela_linux_unsupported("a client address for a shmem-backed mapping: it is refused");
	return (unsigned long)-ENOMEM;
}
/* The locks of every mapping of an address space, which subscribing its
 * first notifier takes. Only a driver mirroring a client's memory (userptr)
 * subscribes one, and that also needs the client's pages faulted in through
 * Linux page tables, which Stela's clients do not have. The subscription is
 * refused, said once, with the error Linux gives when the locks cannot all
 * be taken, and none is then held to drop. */
int mm_take_all_locks(struct mm_struct *mm)
{
	(void)mm;
	stela_linux_unsupported("a client's own memory as a device buffer (userptr)");
	return -EINTR;
}

void mm_drop_all_locks(struct mm_struct *mm)
{
	(void)mm;
}
/* A client's own memory as a device's buffer (userptr) and a value written
 * into a client from a worker (a user fence). Both need the client as a
 * Linux address space with VMAs and page tables. Stela's clients are not:
 * the Metal device, the only caller of a GPU driver's ioctls, asks for
 * neither. A fault of a range is refused and said once. A user fence is
 * refused by xe where it is asked for, with one warning: no Stela task has an
 * address space, and xe refuses a caller without one (third_party/patches/
 * linux, "refuse a user fence"). What follows needs an address space a
 * caller had, so nothing reaches it; were it reached, a search finds no
 * mapping (the caller fails the range with -EFAULT), a worker runs without
 * the address space, so its copy goes through the owner's copy services as
 * any copy to a client does, and a reference put leaves nothing to free,
 * each said once. */
int hmm_range_fault(struct hmm_range *range)
{
	(void)range;
	stela_linux_unsupported("a client's own memory as a device buffer (userptr)");
	return -EOPNOTSUPP;
}
struct vm_area_struct *find_vma_intersection(struct mm_struct *mm, unsigned long start,
					     unsigned long end)
{
	(void)mm;
	(void)start;
	(void)end;
	stela_linux_unsupported("a client's own memory as a device buffer (userptr)");
	return NULL;
}

void kthread_use_mm(struct mm_struct *mm)
{
	(void)mm;
	stela_linux_unsupported("a worker in a client's address space");
}

void kthread_unuse_mm(struct mm_struct *mm)
{
	(void)mm;
}

void mmput(struct mm_struct *mm)
{
	(void)mm;
	stela_linux_unsupported("a worker in a client's address space");
}
/* Register maps, which only a discrete Intel GPU's management controller
 * (an I2C controller behind the GPU's own registers) asks for. Stela has no
 * platform bus for that controller's driver, so the map is refused and said
 * once and xe goes on without the controller (third_party/patches/linux,
 * "management controller"). What follows a map is refused as well, each
 * said once, with the errors Linux gives: the I2C bus's notifier, the
 * controller's software node and its platform device. */
struct regmap *__devm_regmap_init(struct device *dev, const struct regmap_bus *bus,
				  void *bus_context, const struct regmap_config *config,
				  struct lock_class_key *lock_key, const char *lock_name)
{
	(void)dev;
	(void)bus;
	(void)bus_context;
	(void)config;
	(void)lock_key;
	(void)lock_name;
	stela_linux_unsupported("register maps: a GPU's management controller");
	return ERR_PTR(-EOPNOTSUPP);
}
int bus_register_notifier(const struct bus_type *bus, struct notifier_block *notifier)
{
	(void)bus;
	(void)notifier;
	stela_linux_unsupported("a bus notifier: it is refused");
	return -EOPNOTSUPP;
}

/* No notifier is registered, so none is found, as Linux answers. */
int bus_unregister_notifier(const struct bus_type *bus, struct notifier_block *notifier)
{
	(void)bus;
	(void)notifier;
	return -ENOENT;
}

struct fwnode_handle *fwnode_create_software_node(const struct property_entry *properties,
						   const struct fwnode_handle *parent)
{
	(void)properties;
	(void)parent;
	stela_linux_unsupported("a software firmware node: it is refused");
	return ERR_PTR(-EOPNOTSUPP);
}

/* Only a node fwnode_create_software_node made is removed, and it makes none. */
void fwnode_remove_software_node(struct fwnode_handle *node)
{
	(void)node;
}

struct platform_device *platform_device_alloc(const char *name, int id)
{
	(void)name;
	(void)id;
	stela_linux_unsupported("a platform device: it is refused");
	return NULL;
}

int platform_device_add(struct platform_device *device)
{
	(void)device;
	stela_linux_unsupported("a platform device: it is refused");
	return -EOPNOTSUPP;
}

/* Only a device platform_device_alloc gave is put, and it gives none. */
void platform_device_put(struct platform_device *device)
{
	(void)device;
}
/* debugfs's seq_file operations, which no file opens: Stela has no debugfs. */
STELA_UNSUPPORTED(int, single_open, struct file *, int (*)(struct seq_file *, void *), void *)
STELA_UNSUPPORTED(int, single_release, struct inode *, struct file *)
STELA_UNSUPPORTED(ssize_t, seq_read, struct file *, char __user *, size_t, loff_t *)
STELA_UNSUPPORTED(loff_t, seq_lseek, struct file *, loff_t, int)
/* Platform devices, which need the platform bus: only HDMI audio codecs and
 * the partitions of a partitioned accelerator reach DRM as such. */
STELA_UNSUPPORTED(struct platform_device *, platform_device_register_full,
		  const struct platform_device_info *)
/* Only a device platform_device_register_full or platform_device_add gave is
 * unregistered, and neither gives one. */
void platform_device_unregister(struct platform_device *device)
{
	(void)device;
}
/* Whether a frame is RAM is the answer of the firmware's memory map in
 * iomem_resource, which holds no map here: Stela's owners hand Linux its
 * pages. Only the NUMA placement of an accelerator's memory partitions asks. */
STELA_UNSUPPORTED(int, page_is_ram, unsigned long)
