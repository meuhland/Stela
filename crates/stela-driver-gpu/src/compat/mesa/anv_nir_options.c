/* SPDX-License-Identifier: GPL-2.0-or-later */
/* The NIR options ANV builds a device's shaders of a stage with, for the AIR
 * compiler, which reaches ANV by no route Vulkan offers. BRW keeps one set
 * per stage; a stage past its table has none. */
#include "anv_private.h"

const struct nir_shader_compiler_options *stela_anv_nir_options(VkPhysicalDevice physical,
								unsigned stage)
{
	ANV_FROM_HANDLE(anv_physical_device, pdevice, physical);
	if (stage >= MESA_ALL_SHADER_STAGES)
		return NULL;
	return &pdevice->compiler->nir_options[stage];
}
