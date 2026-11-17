// SPDX-License-Identifier: GPL-2.0-or-later
/* Kernel-mode floating-point sections over the production compat/kernel/fpu.c:
 * the section owns the control word and holds preemption off. The arithmetic
 * of a real floating-point unit is exercised where one is built (amdgpu's
 * display mode library). */
#include <linux/fpu.h>
#include <linux/preempt.h>
#include <stela/linux.h>
#include <stela/test.h>

static u32 mxcsr(void)
{
	u32 value;
	asm volatile("stmxcsr %0" : "=m"(value));
	return value;
}

static void set_mxcsr(u32 value) { asm volatile("ldmxcsr %0" : : "m"(value)); }

int main(void)
{
	/* Round toward zero with a stale exception flag: what a caller of
	 * Linux may carry. */
	const u32 entered = 0x1f80u | 0x6000u | 0x0001u;

	stela_linux_enter(&stela_linux_bare);
	assert(kernel_fpu_available());
	for (int pass = 0; pass < 2; pass++) {
		int count = preempt_count();

		set_mxcsr(entered);
		kernel_fpu_begin();
		assert(mxcsr() == 0x1f80u);
		assert(preempt_count() == count + 1);
		/* Flags the section raises do not reach its caller. */
		set_mxcsr(0x1f80u | 0x0020u);
		kernel_fpu_end();
		assert(mxcsr() == entered);
		assert(preempt_count() == count);
	}
	set_mxcsr(0x1f80u);
	stela_linux_exit();
	printf("Linux kernel FPU: sections own MXCSR and hold preemption off\n");
	return 0;
}
