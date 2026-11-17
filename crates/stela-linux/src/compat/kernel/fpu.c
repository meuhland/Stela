// SPDX-License-Identifier: GPL-2.0-or-later
/* Kernel-mode floating-point sections (asm/fpu.h). */
#include <linux/bug.h>
#include <linux/fpu.h>
#include <linux/preempt.h>

/* x86's MXCSR_DEFAULT: round to nearest, every exception masked, no flag. */
#define STELA_MXCSR_DEFAULT 0x1f80u

/* The control word of whoever entered the open section. One processor runs
 * Linux and a section cannot be left for another task, so there is at most
 * one. */
static u32 entered_mxcsr;
static bool in_section;

void kernel_fpu_begin(void)
{
	u32 mxcsr = STELA_MXCSR_DEFAULT;

	preempt_disable();
	/* x86 warns the same way: sections do not nest. */
	WARN_ON_ONCE(in_section);
	in_section = true;
	asm volatile("stmxcsr %0" : "=m"(entered_mxcsr));
	asm volatile("ldmxcsr %0" : : "m"(mxcsr));
}

void kernel_fpu_end(void)
{
	WARN_ON_ONCE(!in_section);
	/* Also drops the exception flags the section raised. */
	asm volatile("ldmxcsr %0" : : "m"(entered_mxcsr));
	in_section = false;
	preempt_enable();
}
