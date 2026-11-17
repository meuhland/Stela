/* SPDX-License-Identifier: GPL-2.0-or-later */
/* The NIR options a device's shaders are built with, for the AIR compiler,
 * which reaches NVK by no route Vulkan offers. NAK has one set for every
 * stage. */
#include "nak.h"
#include "nvk_physical_device.h"

const struct nir_shader_compiler_options *stela_nvk_nir_options(VkPhysicalDevice physical,
								unsigned stage)
{
	(void)stage;
	VK_FROM_HANDLE(nvk_physical_device, pdev, physical);
	return nak_nir_options(pdev->nak);
}
