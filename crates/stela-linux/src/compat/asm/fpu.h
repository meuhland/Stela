/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Kernel-mode floating point on Stela's arch (linux/fpu.h and Linux's
 * Documentation/core-api/floating-point.rst), as x86's asm/fpu.h gives it.
 *
 * Every unit is built for the general registers only; a unit that computes
 * in floating point is built with CC_FLAGS_FPU instead and may be called only
 * between kernel_fpu_begin() and kernel_fpu_end(). Linux runs here as plain
 * calls on one processor: whoever calls into it follows the C calling
 * convention, which already treats the caller-saved vector registers as lost
 * and makes the compiler of the floating-point unit keep the rest. So the
 * section has no register file to save. What it must own is the control
 * word: the processor may carry another rounding mode or exception mask
 * when Linux is entered, and Linux's floating-point code is written for the
 * default one. The section also may not be left for another task, which
 * disabling preemption guarantees as on x86 (kernel/fpu.c). */
#ifndef _ASM_STELA_FPU_H
#define _ASM_STELA_FPU_H

#include <linux/types.h>

#define kernel_fpu_available() true
void kernel_fpu_begin(void);
void kernel_fpu_end(void);

#endif
