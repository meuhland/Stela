/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Linux's futex operations that the C library's syscall answers. */
#ifndef STELA_LIBC_LINUX_FUTEX_H
#define STELA_LIBC_LINUX_FUTEX_H
#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define FUTEX_WAIT_BITSET 9
#define FUTEX_WAKE_BITSET 10
#define FUTEX_PRIVATE_FLAG 128
#define FUTEX_CLOCK_REALTIME 256
#define FUTEX_BITSET_MATCH_ANY 0xffffffff
#endif
