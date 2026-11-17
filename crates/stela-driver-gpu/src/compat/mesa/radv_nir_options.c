/* SPDX-License-Identifier: GPL-2.0-or-later */
/* The NIR options RADV builds a device's shaders of a stage with, for the AIR
 * compiler, which reaches RADV by no route Vulkan offers. RADV keeps them in
 * each logical device's compiler information, made at device creation from
 * what the physical device holds: its compiler information, whether it
 * compiles with LLVM and the instance's split-FMA setting. These are made the
 * same way, by RADV's own radv_get_nir_options, from the same inputs. Equal
 * inputs give equal options, so one set is kept per distinct input and lasts
 * the image's life, longer than any physical device. */
#include <stdio.h>
#include <string.h>

#include "radv_instance.h"
#include "radv_physical_device.h"
#include "radv_shader.h"
#include "util/simple_mtx.h"

/* Distinct kinds of AMD GPU one image keeps options for. */
#define KINDS 8

static struct kind {
	struct ac_compiler_info ac;
	bool use_llvm;
	bool split_fma;
	struct radv_compiler_info info;
} kinds[KINDS];
static unsigned kept;
static simple_mtx_t lock = SIMPLE_MTX_INITIALIZER;

static const struct kind *kind_of(const struct radv_physical_device *pdev)
{
	const struct radv_instance *instance = radv_physical_device_instance(pdev);
	const bool split_fma = instance->drirc.debug.split_fma;
	for (unsigned index = 0; index < kept; index++) {
		const struct kind *kind = &kinds[index];
		if (!memcmp(&kind->ac, &pdev->info.compiler_info, sizeof(kind->ac)) &&
		    kind->use_llvm == pdev->use_llvm && kind->split_fma == split_fma)
			return kind;
	}
	if (kept == KINDS) {
		fprintf(stderr, "radv: NIR options are kept for %u kinds of GPU; this one has none\n",
			KINDS);
		return NULL;
	}
	struct kind *kind = &kinds[kept];
	kind->ac = pdev->info.compiler_info;
	kind->use_llvm = pdev->use_llvm;
	kind->split_fma = split_fma;
	kind->info = (struct radv_compiler_info){
		.ac = &kind->ac,
		.key = {.use_llvm = pdev->use_llvm, .split_fma = split_fma},
	};
	radv_get_nir_options(&kind->info);
	kept++;
	return kind;
}

const struct nir_shader_compiler_options *stela_radv_nir_options(VkPhysicalDevice physical,
								 unsigned stage)
{
	VK_FROM_HANDLE(radv_physical_device, pdev, physical);
	if (stage >= MESA_VULKAN_SHADER_STAGES)
		return NULL;
	simple_mtx_lock(&lock);
	const struct kind *kind = kind_of(pdev);
	simple_mtx_unlock(&lock);
	return kind ? &kind->info.nir_options[stage] : NULL;
}
