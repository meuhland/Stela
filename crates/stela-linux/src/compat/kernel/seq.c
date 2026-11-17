// SPDX-License-Identifier: GPL-2.0-only
/* What Linux's text buffers need beyond fs/seq_output.c and the unchanged
 * lib/seq_buf.c, following Linux cc9de5d4b5ab6459f5774946aa3195ad51236fa8
 * fs/seq_file.c. */
#include <linux/seq_file.h>

void seq_putc(struct seq_file *m, char c)
{
	if (m->count < m->size)
		m->buf[m->count++] = c;
}
