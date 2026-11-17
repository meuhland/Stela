// SPDX-License-Identifier: GPL-2.0-or-later
/* A function's own resets on the fixture's real Runtime task, which their
 * settling sleeps need: the function-level reset, the reset through D3hot,
 * and the header Linux restores after either. */
#include <linux/errno.h>
#include <linux/jiffies.h>
#include <linux/pci.h>
#include <linux/string.h>
#include <stela/test.h>

void stela_linux_pci_reset_test(void);

enum { PM = 0x50, EXPRESS = 0x60 };

static u8 config[256];
static unsigned int function_level_resets, power_resets, not_ready;
static bool pending;

/* What a reset does to the function: its header's writable part returns to
 * zero and it does not answer for the next `not_ready` reads. */
static void reset(void)
{
	memset(&config[PCI_COMMAND], 0, 2);
	memset(&config[PCI_BASE_ADDRESS_0], 0, 24);
	config[PCI_INTERRUPT_LINE] = 0;
	not_ready = 2;
}

static int read_config(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 *value)
{
	(void)bus;
	(void)devfn;
	if (where == PCI_COMMAND && not_ready) {
		not_ready--;
		*value = ~0U;
		return 0;
	}
	*value = 0;
	memcpy(value, &config[where], (size_t)size);
	/* A transaction is pending at the first look and done at the next. */
	if (where == EXPRESS + PCI_EXP_DEVSTA && pending) {
		*value |= PCI_EXP_DEVSTA_TRPND;
		pending = false;
	}
	return 0;
}

static int write_config(struct pci_bus *bus, unsigned int devfn, int where, int size, u32 value)
{
	(void)bus;
	(void)devfn;
	if (where == EXPRESS + PCI_EXP_DEVCTL && (value & PCI_EXP_DEVCTL_BCR_FLR)) {
		/* The bit reads back zero; quiet first, as Linux requires. */
		assert(!(config[PCI_COMMAND] & (PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER)));
		assert(!pending);
		function_level_resets++;
		reset();
		return 0;
	}
	if (where == PM + PCI_PM_CTRL) {
		u16 before = *(u16 *)&config[where] & PCI_PM_CTRL_STATE_MASK;
		u16 after = value & PCI_PM_CTRL_STATE_MASK;

		/* D3hot to D0 without No_Soft_Reset is a reset. */
		if (before == 3 && after == 0 && !(config[where] & PCI_PM_CTRL_NO_SOFT_RESET)) {
			power_resets++;
			reset();
		}
	}
	memcpy(&config[where], &value, (size_t)size);
	return 0;
}

static struct pci_ops ops = {.read = read_config, .write = write_config};

static void program_header(void)
{
	*(u16 *)&config[PCI_COMMAND] = PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER;
	*(u32 *)&config[PCI_BASE_ADDRESS_0] = 0xfc000000;
	*(u32 *)&config[PCI_BASE_ADDRESS_5] = 0xfcb00000;
	config[PCI_INTERRUPT_LINE] = 11;
}

static void header_is_programmed(void)
{
	assert(*(u16 *)&config[PCI_COMMAND] == (PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER));
	assert(*(u32 *)&config[PCI_BASE_ADDRESS_0] == 0xfc000000);
	assert(*(u32 *)&config[PCI_BASE_ADDRESS_5] == 0xfcb00000);
	assert(config[PCI_INTERRUPT_LINE] == 11);
}

void stela_linux_pci_reset_test(void)
{
	static struct pci_bus bus = {.ops = &ops, .number = 0x10};
	static struct pci_dev dev;

	dev.bus = &bus;
	dev.devfn = PCI_DEVFN(0, 0);
	dev.hdr_type = PCI_HEADER_TYPE_NORMAL;
	dev.current_state = PCI_D0;
	*(u16 *)&config[PCI_VENDOR_ID] = 0x1002;
	*(u16 *)&config[PCI_STATUS] = PCI_STATUS_CAP_LIST;
	config[PCI_CAPABILITY_LIST] = PM;
	config[PM] = PCI_CAP_ID_PM;
	config[PM + 1] = EXPRESS;
	config[EXPRESS] = PCI_CAP_ID_EXP;
	*(u16 *)&config[EXPRESS + PCI_EXP_FLAGS] = 2 | PCI_EXP_TYPE_ENDPOINT << 4;
	*(u32 *)&config[EXPRESS + PCI_EXP_DEVCAP] = PCI_EXP_DEVCAP_FLR;
	dev.pcie_cap = EXPRESS;
	dev.pcie_flags_reg = *(u16 *)&config[EXPRESS + PCI_EXP_FLAGS];
	dev.devcap = PCI_EXP_DEVCAP_FLR;

	/* With a function-level reset: Linux waits for the transaction that is
	 * pending, resets once, waits out the reads the function does not
	 * answer and restores the header. The power-state reset is not used. */
	program_header();
	pending = true;
	unsigned long start = jiffies;
	assert(!pci_reset_function(&dev));
	assert(function_level_resets == 1 && !power_resets && !not_ready && !dev.state_saved);
	/* pcie_flr's 100 ms after the reset, at least. */
	assert(time_after_eq(jiffies, start + msecs_to_jiffies(100)));
	header_is_programmed();

	/* Without one, a D3hot that resets the function is the method, and the
	 * function is left in D0 with its header restored. */
	dev.devcap = 0;
	assert(!pci_reset_function(&dev));
	assert(function_level_resets == 1 && power_resets == 1 && !not_ready);
	assert(!(*(u16 *)&config[PM + PCI_PM_CTRL] & PCI_PM_CTRL_STATE_MASK));
	header_is_programmed();

	/* A function whose D3hot keeps its state has no reset of its own. */
	*(u16 *)&config[PM + PCI_PM_CTRL] = PCI_PM_CTRL_NO_SOFT_RESET;
	assert(pci_reset_function(&dev) == -ENOTTY && power_resets == 1);
	header_is_programmed();
}
