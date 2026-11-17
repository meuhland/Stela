/* SPDX-License-Identifier: GPL-2.0-or-later */
/* The NIR compute shader anv_device.c hands ANV in place of SPIR-V, as the
 * Metal device hands the AIR compiler's NIR (`metal::native::objects::NirStage`):
 * built with the NIR options ANV gives the AIR compiler, it reads a texel of
 * the storage image at set 0, binding 0 without a format, as a Metal
 * read-write texture read does, and writes it to the storage image at
 * binding 1. */
#include "nir.h"
#include "nir_builder.h"

static nir_variable *image(nir_shader *shader, unsigned binding, const char *name)
{
	nir_variable *variable = nir_variable_create(
		shader, nir_var_image, glsl_image_type(GLSL_SAMPLER_DIM_2D, false, GLSL_TYPE_FLOAT),
		name);
	variable->data.descriptor_set = 0;
	variable->data.binding = binding;
	variable->data.image.format = PIPE_FORMAT_NONE;
	return variable;
}

void *stela_anv_test_nir(const void *options)
{
	nir_builder builder = nir_builder_init_simple_shader(
		MESA_SHADER_COMPUTE, (const nir_shader_compiler_options *)options, "anv_nir_copy");
	nir_builder *b = &builder;
	b->shader->info.workgroup_size[0] = 8;
	b->shader->info.workgroup_size[1] = 8;
	b->shader->info.workgroup_size[2] = 1;
	nir_variable *source = image(b->shader, 0, "source");
	nir_variable *destination = image(b->shader, 1, "destination");
	nir_def *id = nir_load_global_invocation_id(b, 32);
	nir_def *coordinates =
		nir_vec4(b, nir_channel(b, id, 0), nir_channel(b, id, 1), nir_undef(b, 1, 32),
			 nir_undef(b, 1, 32));
	nir_def *texel = nir_image_deref_load(
		b, 4, 32, &nir_build_deref_var(b, source)->def, coordinates, nir_undef(b, 1, 32),
		nir_imm_int(b, 0), .image_dim = GLSL_SAMPLER_DIM_2D, .format = PIPE_FORMAT_NONE,
		.access = ACCESS_NON_WRITEABLE, .dest_type = nir_type_float32);
	nir_image_deref_store(b, &nir_build_deref_var(b, destination)->def, coordinates,
			      nir_undef(b, 1, 32), texel, nir_imm_int(b, 0),
			      .image_dim = GLSL_SAMPLER_DIM_2D, .format = PIPE_FORMAT_NONE,
			      .access = ACCESS_NON_READABLE, .src_type = nir_type_float32);
	return b->shader;
}

void stela_anv_test_nir_free(void *shader)
{
	ralloc_free(shader);
}
