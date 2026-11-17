// SPDX-License-Identifier: GPL-2.0-or-later
/* Driver initcalls run after the original Initial resource bootstrap.
 * Stop at the first failed stage and preserve its error. The command line
 * is the kernel's: `length` bytes of space-separated `module.parameter=value`
 * words without a NUL, which the registry copies and parses into the
 * built-in modules' parameters before any initcall, as start_kernel does. A
 * word no module declares fails the start. */
#include <stela/registry.h>

int stela_linux_start(const struct stela_linux_registry_image *image,
		      const char *command_line, size_t length,
		      struct stela_linux_registry **result)
{
	int error = stela_linux_registry_begin(image, command_line, length, result);

	if (!error)
		error = stela_linux_registry_early_parameters(*result, NULL);
	if (!error)
		error = stela_linux_registry_initial_parameters(*result, NULL, NULL);
	for (unsigned int stage = 0; !error && stage < STELA_LINUX_STAGE_DONE; stage++)
		error = stela_linux_registry_run_stage(*result, stage, NULL);
	return error;
}
