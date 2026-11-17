// SPDX-License-Identifier: GPL-2.0-only
/* Linux's power supply class as far as a driver asks it, following Linux
 * cc9de5d4b5ab6459f5774946aa3195ad51236fa8
 * drivers/power/supply/power_supply_core.c. A supply is registered by the
 * driver of a battery or of a mains adapter, and Linux here holds neither,
 * so the class is empty. */
#include <linux/power_supply.h>

/* Whether the system runs on mains. Linux counts the supplies of system
 * scope and, finding none, answers that it does: "most probably we are
 * running on a desktop system". With no supply registered that is the
 * answer for every machine, a laptop on its battery included. */
int power_supply_is_system_supplied(void)
{
	return 1;
}
