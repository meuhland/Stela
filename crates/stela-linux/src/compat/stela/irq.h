/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_IRQ_H
#define STELA_IRQ_H
#include <linux/types.h>

/* Run the handlers of a raised line, on the owner's service task. */
void stela_linux_irq_run(unsigned int irq);
bool stela_linux_irq_actions_joined(unsigned int irq);
/* Owners number the lines they route below this. Lines a driver maps in an
 * IRQ domain are numbered from it up (arch_dynirq_lower_bound), so the two
 * never meet. */
#define STELA_LINUX_OWNER_LINES 256U
#endif
