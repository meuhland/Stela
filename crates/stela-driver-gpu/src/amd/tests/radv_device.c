/* SPDX-License-Identifier: GPL-2.0-or-later */
/* RADV on amdgpu with no GPU: the image's route to a device, run on the host.
 * The program links RADV's objects as Mesa links its driver, Stela's libdrm
 * stand-in (compat/mesa/drm.c) in place of libdrm, libdrm_amdgpu from its
 * unchanged sources, and Stela's NIR options hook
 * (compat/mesa/radv_nir_options.c), and runs over Mesa's amdgpu no-op DRM
 * shim preloaded, which shows the device AMDGPU_GPU_ID names. It creates the
 * instance and the device of the one GPU the shim shows, checks what Vulkan
 * reports of it, asks the hook for each stage's NIR options, builds a compute
 * pipeline (ACO compiles it; its statistics count the instructions and code
 * bytes), and records, submits and waits for one dispatch, which the shim
 * accepts without running it. It then builds a second compute pipeline from
 * NIR made with the hook's options and given in
 * VkPipelineShaderStageNirCreateInfoMESA, as the Metal device gives every
 * shader, and checks that ACO compiled it and that RADV left the caller's NIR
 * as it was.
 *
 * The program is linked with --wrap=ioctl, so every ioctl of the linked code
 * passes through here and each amdgpu ioctl is counted.
 *
 * Usage: radv_device integrated|discrete
 * Prints "RADV on amdgpu: ..." and "amdgpu ioctls: NAME=count ..." and exits
 * 0, or names the failed check. */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>

#include <vulkan/vulkan.h>

#include <stela/host.h>

#include "drm-uapi/amdgpu_drm.h"
#include "nir_builder.h"
#include "radv_device_shader.h"
#include "vk_internal_exts.h"

#define CHECK(condition)                                                                       \
	do {                                                                                   \
		if (!(condition)) {                                                            \
			fprintf(stderr, "radv_device: %s:%d: %s\n", __FILE__, __LINE__, #condition); \
			exit(1);                                                               \
		}                                                                              \
	} while (0)
#define VK(call) CHECK((call) == VK_SUCCESS)

/* Mesa's shader stages up to compute, past whose count of every stage the
 * hook answers none. */
#define STAGES (MESA_SHADER_COMPUTE + 1)

/* RADV's loader entry under Mesa's name: the host build holds one driver,
 * where the image compiles it as radv_icdGetInstanceProcAddr. */
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vk_icdGetInstanceProcAddr(VkInstance, const char *);
const void *stela_radv_nir_options(VkPhysicalDevice physical, unsigned stage);

/* amdgpu's ioctls by number, with how often the linked code made each. */
static const char *const names[] = {
	[DRM_AMDGPU_GEM_CREATE] = "GEM_CREATE",
	[DRM_AMDGPU_GEM_MMAP] = "GEM_MMAP",
	[DRM_AMDGPU_CTX] = "CTX",
	[DRM_AMDGPU_BO_LIST] = "BO_LIST",
	[DRM_AMDGPU_CS] = "CS",
	[DRM_AMDGPU_INFO] = "INFO",
	[DRM_AMDGPU_GEM_METADATA] = "GEM_METADATA",
	[DRM_AMDGPU_GEM_WAIT_IDLE] = "GEM_WAIT_IDLE",
	[DRM_AMDGPU_GEM_VA] = "GEM_VA",
	[DRM_AMDGPU_WAIT_CS] = "WAIT_CS",
	[DRM_AMDGPU_GEM_OP] = "GEM_OP",
	[DRM_AMDGPU_GEM_USERPTR] = "GEM_USERPTR",
	[DRM_AMDGPU_WAIT_FENCES] = "WAIT_FENCES",
	[DRM_AMDGPU_VM] = "VM",
	[DRM_AMDGPU_FENCE_TO_HANDLE] = "FENCE_TO_HANDLE",
	[DRM_AMDGPU_SCHED] = "SCHED",
	[DRM_AMDGPU_USERQ] = "USERQ",
	[DRM_AMDGPU_USERQ_SIGNAL] = "USERQ_SIGNAL",
	[DRM_AMDGPU_USERQ_WAIT] = "USERQ_WAIT",
};
static unsigned counts[DRM_COMMAND_END - DRM_COMMAND_BASE];

int __real_ioctl(int fd, unsigned long request, ...);
int __wrap_ioctl(int fd, unsigned long request, ...);

int __wrap_ioctl(int fd, unsigned long request, ...)
{
	va_list arguments;
	va_start(arguments, request);
	void *argument = va_arg(arguments, void *);
	va_end(arguments);
	unsigned number = _IOC_NR(request);
	if (_IOC_TYPE(request) == DRM_IOCTL_BASE && number >= DRM_COMMAND_BASE &&
	    number < DRM_COMMAND_END)
		__atomic_fetch_add(&counts[number - DRM_COMMAND_BASE], 1, __ATOMIC_RELAXED);
	return __real_ioctl(fd, request, argument);
}

/* How often the libdrm stand-in asked for the device: never would mean
 * another libdrm listed it. */
static unsigned listed;

/* What the image's Linux side answers through `stela_libc_drm_device`: the
 * one render node the shim shows. RADV takes the device's own IDs from
 * amdgpu's queries, not from this. */
unsigned int stela_libc_drm_devices(void)
{
	return 1;
}

void stela_libc_drm_device(unsigned int index, struct stela_libc_drm_device *device)
{
	CHECK(index == 0);
	listed++;
	memset(device, 0, sizeof(*device));
	device->vendor = 0x1002;
	device->device = 0x15e7;
	device->bus = 4;
}

/* What ACO made of each of a pipeline's executables: its instructions and
 * code bytes, from the pipeline's statistics. */
static void aco_code(VkDevice device, VkPipeline pipeline,
		     PFN_vkGetPipelineExecutablePropertiesKHR properties,
		     PFN_vkGetPipelineExecutableStatisticsKHR statistics_of, uint64_t *instructions,
		     uint64_t *code_bytes)
{
	const VkPipelineInfoKHR pipeline_info = {
		.sType = VK_STRUCTURE_TYPE_PIPELINE_INFO_KHR,
		.pipeline = pipeline,
	};
	uint32_t executable_count = 0;
	VK(properties(device, &pipeline_info, &executable_count, NULL));
	CHECK(executable_count >= 1);
	*instructions = 0;
	*code_bytes = 0;
	for (uint32_t executable = 0; executable < executable_count; executable++) {
		const VkPipelineExecutableInfoKHR info = {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_INFO_KHR,
			.pipeline = pipeline,
			.executableIndex = executable,
		};
		VkPipelineExecutableStatisticKHR statistics[64];
		uint32_t statistic_count = 64;
		for (uint32_t i = 0; i < statistic_count; i++)
			statistics[i] = (VkPipelineExecutableStatisticKHR){
				.sType = VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_STATISTIC_KHR,
			};
		VkResult result = statistics_of(device, &info, &statistic_count, statistics);
		CHECK(result == VK_SUCCESS || result == VK_INCOMPLETE);
		for (uint32_t i = 0; i < statistic_count; i++) {
			if (statistics[i].format != VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_UINT64_KHR)
				continue;
			if (!strcmp(statistics[i].name, "Instructions"))
				*instructions += statistics[i].value.u64;
			else if (!strcmp(statistics[i].name, "Code size"))
				*code_bytes += statistics[i].value.u64;
		}
	}
}

/* The SPIR-V shader's work as the Metal device's compiler gives it: NIR made
 * with the device's options, its buffer reached through a 64-bit address in
 * push constants, as Metal's buffers are, with no descriptor. */
static nir_shader *nir_shader_of(const nir_shader_compiler_options *options)
{
	nir_builder b = nir_builder_init_simple_shader(MESA_SHADER_COMPUTE, options, "stela_radv_nir");
	b.shader->info.workgroup_size[0] = 64;
	nir_def *constants = nir_load_push_constant(&b, 2, 32, nir_imm_int(&b, 0), .range = 8);
	nir_def *base = nir_pack_64_2x32(&b, constants);
	nir_def *index =
		nir_iadd(&b, nir_imul_imm(&b, nir_channel(&b, nir_load_workgroup_id(&b), 0), 64),
			 nir_load_local_invocation_index(&b));
	nir_def *address = nir_iadd(&b, base, nir_u2u64(&b, nir_imul_imm(&b, index, 4)));
	nir_def *value = nir_load_global(&b, 1, 32, address, .align_mul = 4);
	nir_store_global(&b, nir_iadd(&b, nir_imul_imm(&b, value, 3), index), address,
			 .align_mul = 4);
	return b.shader;
}

#define LOAD(instance, name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)

int main(int argc, char **argv)
{
	CHECK(argc == 2);
	VkPhysicalDeviceType expected_type = !strcmp(argv[1], "discrete")
						     ? VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
						     : VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;

	LOAD(NULL, vkCreateInstance);
	const VkApplicationInfo application = {
		.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.apiVersion = VK_API_VERSION_1_3,
	};
	VkInstance instance;
	VK(vkCreateInstance(
		&(VkInstanceCreateInfo){
			.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
			.pApplicationInfo = &application,
		},
		NULL, &instance));
	LOAD(instance, vkEnumeratePhysicalDevices);
	LOAD(instance, vkGetPhysicalDeviceProperties2);
	LOAD(instance, vkGetPhysicalDeviceQueueFamilyProperties);
	LOAD(instance, vkGetPhysicalDeviceMemoryProperties);
	LOAD(instance, vkCreateDevice);
	LOAD(instance, vkGetDeviceProcAddr);
	LOAD(instance, vkDestroyInstance);

	uint32_t count = 0;
	VK(vkEnumeratePhysicalDevices(instance, &count, NULL));
	CHECK(count == 1);
	VkPhysicalDevice physical;
	VK(vkEnumeratePhysicalDevices(instance, &count, &physical));
	CHECK(listed > 0);

	VkPhysicalDeviceDriverProperties driver = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
	};
	VkPhysicalDeviceProperties2 properties = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
		.pNext = &driver,
	};
	vkGetPhysicalDeviceProperties2(physical, &properties);
	CHECK(properties.properties.vendorID == 0x1002);
	CHECK(properties.properties.deviceType == expected_type);
	CHECK(driver.driverID == VK_DRIVER_ID_MESA_RADV);

	/* The hook answers one set per stage, and none past Mesa's stages. */
	const void *options[STAGES];
	for (unsigned stage = 0; stage < STAGES; stage++) {
		options[stage] = stela_radv_nir_options(physical, stage);
		CHECK(options[stage]);
		for (unsigned other = 0; other < stage; other++)
			CHECK(options[other] != options[stage]);
	}
	CHECK(!stela_radv_nir_options(physical, MESA_ALL_SHADER_STAGES));

	uint32_t families = 0;
	vkGetPhysicalDeviceQueueFamilyProperties(physical, &families, NULL);
	VkQueueFamilyProperties family_properties[8];
	CHECK(families && families <= 8);
	vkGetPhysicalDeviceQueueFamilyProperties(physical, &families, family_properties);
	uint32_t family = 0;
	while (family < families && !(family_properties[family].queueFlags & VK_QUEUE_COMPUTE_BIT))
		family++;
	CHECK(family < families);

	VkPhysicalDevicePipelineExecutablePropertiesFeaturesKHR executables = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_EXECUTABLE_PROPERTIES_FEATURES_KHR,
		.pipelineExecutableInfo = VK_TRUE,
	};
	const char *extensions[] = {VK_KHR_PIPELINE_EXECUTABLE_PROPERTIES_EXTENSION_NAME};
	VkDevice device;
	VK(vkCreateDevice(physical,
			  &(VkDeviceCreateInfo){
				  .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
				  .pNext = &executables,
				  .queueCreateInfoCount = 1,
				  .pQueueCreateInfos =
					  &(VkDeviceQueueCreateInfo){
						  .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
						  .queueFamilyIndex = family,
						  .queueCount = 1,
						  .pQueuePriorities = (float[]){1.0f},
					  },
				  .enabledExtensionCount = 1,
				  .ppEnabledExtensionNames = extensions,
			  },
			  NULL, &device));
#define DEVICE(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)
	DEVICE(vkGetDeviceQueue);
	DEVICE(vkCreateShaderModule);
	DEVICE(vkCreateDescriptorSetLayout);
	DEVICE(vkCreatePipelineLayout);
	DEVICE(vkCreateComputePipelines);
	DEVICE(vkGetPipelineExecutablePropertiesKHR);
	DEVICE(vkGetPipelineExecutableStatisticsKHR);
	DEVICE(vkCreateBuffer);
	DEVICE(vkGetBufferMemoryRequirements);
	DEVICE(vkAllocateMemory);
	DEVICE(vkBindBufferMemory);
	DEVICE(vkMapMemory);
	DEVICE(vkCreateDescriptorPool);
	DEVICE(vkAllocateDescriptorSets);
	DEVICE(vkUpdateDescriptorSets);
	DEVICE(vkCreateCommandPool);
	DEVICE(vkAllocateCommandBuffers);
	DEVICE(vkBeginCommandBuffer);
	DEVICE(vkCmdBindPipeline);
	DEVICE(vkCmdBindDescriptorSets);
	DEVICE(vkCmdDispatch);
	DEVICE(vkEndCommandBuffer);
	DEVICE(vkCreateFence);
	DEVICE(vkQueueSubmit);
	DEVICE(vkWaitForFences);
	DEVICE(vkDeviceWaitIdle);
	DEVICE(vkDestroyFence);
	DEVICE(vkDestroyCommandPool);
	DEVICE(vkDestroyDescriptorPool);
	DEVICE(vkFreeMemory);
	DEVICE(vkDestroyBuffer);
	DEVICE(vkDestroyPipeline);
	DEVICE(vkDestroyPipelineLayout);
	DEVICE(vkDestroyDescriptorSetLayout);
	DEVICE(vkDestroyShaderModule);
	DEVICE(vkDestroyDevice);
	VkQueue queue;
	vkGetDeviceQueue(device, family, 0, &queue);

	VkShaderModule module;
	VK(vkCreateShaderModule(device,
				&(VkShaderModuleCreateInfo){
					.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
					.codeSize = sizeof(stela_radv_device_spirv),
					.pCode = stela_radv_device_spirv,
				},
				NULL, &module));
	VkDescriptorSetLayout set_layout;
	VK(vkCreateDescriptorSetLayout(
		device,
		&(VkDescriptorSetLayoutCreateInfo){
			.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
			.bindingCount = 1,
			.pBindings =
				&(VkDescriptorSetLayoutBinding){
					.binding = 0,
					.descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.descriptorCount = 1,
					.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
				},
		},
		NULL, &set_layout));
	VkPipelineLayout layout;
	VK(vkCreatePipelineLayout(device,
				  &(VkPipelineLayoutCreateInfo){
					  .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
					  .setLayoutCount = 1,
					  .pSetLayouts = &set_layout,
				  },
				  NULL, &layout));
	VkPipeline pipeline;
	VK(vkCreateComputePipelines(
		device, VK_NULL_HANDLE, 1,
		&(VkComputePipelineCreateInfo){
			.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
			.flags = VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR,
			.stage =
				{
					.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
					.stage = VK_SHADER_STAGE_COMPUTE_BIT,
					.module = module,
					.pName = "main",
				},
			.layout = layout,
		},
		NULL, &pipeline));

	/* ACO's code: each executable's instruction count and code bytes. */
	uint64_t instructions, code_bytes;
	aco_code(device, pipeline, vkGetPipelineExecutablePropertiesKHR,
		 vkGetPipelineExecutableStatisticsKHR, &instructions, &code_bytes);
	CHECK(instructions > 0 && code_bytes > 0);

	/* One dispatch over a host-visible buffer, submitted and waited for. */
	const VkDeviceSize bytes = 64 * sizeof(uint32_t);
	VkBuffer buffer;
	VK(vkCreateBuffer(device,
			  &(VkBufferCreateInfo){
				  .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
				  .size = bytes,
				  .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
			  },
			  NULL, &buffer));
	VkMemoryRequirements requirements;
	vkGetBufferMemoryRequirements(device, buffer, &requirements);
	VkPhysicalDeviceMemoryProperties memory;
	vkGetPhysicalDeviceMemoryProperties(physical, &memory);
	uint32_t type = 0;
	const VkMemoryPropertyFlags visible =
		VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
	while (type < memory.memoryTypeCount &&
	       (!(requirements.memoryTypeBits & (1u << type)) ||
		(memory.memoryTypes[type].propertyFlags & visible) != visible))
		type++;
	CHECK(type < memory.memoryTypeCount);
	VkDeviceMemory allocation;
	VK(vkAllocateMemory(device,
			    &(VkMemoryAllocateInfo){
				    .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				    .allocationSize = requirements.size,
				    .memoryTypeIndex = type,
			    },
			    NULL, &allocation));
	VK(vkBindBufferMemory(device, buffer, allocation, 0));
	uint32_t *values;
	VK(vkMapMemory(device, allocation, 0, bytes, 0, (void **)&values));
	for (uint32_t i = 0; i < 64; i++)
		values[i] = i;

	VkDescriptorPool pool;
	VK(vkCreateDescriptorPool(device,
				  &(VkDescriptorPoolCreateInfo){
					  .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
					  .maxSets = 1,
					  .poolSizeCount = 1,
					  .pPoolSizes =
						  &(VkDescriptorPoolSize){
							  .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
							  .descriptorCount = 1,
						  },
				  },
				  NULL, &pool));
	VkDescriptorSet set;
	VK(vkAllocateDescriptorSets(device,
				    &(VkDescriptorSetAllocateInfo){
					    .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
					    .descriptorPool = pool,
					    .descriptorSetCount = 1,
					    .pSetLayouts = &set_layout,
				    },
				    &set));
	vkUpdateDescriptorSets(device, 1,
			       &(VkWriteDescriptorSet){
				       .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
				       .dstSet = set,
				       .descriptorCount = 1,
				       .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
				       .pBufferInfo =
					       &(VkDescriptorBufferInfo){
						       .buffer = buffer,
						       .range = bytes,
					       },
			       },
			       0, NULL);
	VkCommandPool commands;
	VK(vkCreateCommandPool(device,
			       &(VkCommandPoolCreateInfo){
				       .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				       .queueFamilyIndex = family,
			       },
			       NULL, &commands));
	VkCommandBuffer command_buffer;
	VK(vkAllocateCommandBuffers(device,
				    &(VkCommandBufferAllocateInfo){
					    .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
					    .commandPool = commands,
					    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
					    .commandBufferCount = 1,
				    },
				    &command_buffer));
	VK(vkBeginCommandBuffer(command_buffer,
				&(VkCommandBufferBeginInfo){
					.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
					.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
				}));
	vkCmdBindPipeline(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
	vkCmdBindDescriptorSets(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set,
				0, NULL);
	vkCmdDispatch(command_buffer, 1, 1, 1);
	VK(vkEndCommandBuffer(command_buffer));
	VkFence fence;
	VK(vkCreateFence(device,
			 &(VkFenceCreateInfo){
				 .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
			 },
			 NULL, &fence));
	VK(vkQueueSubmit(queue, 1,
			 &(VkSubmitInfo){
				 .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
				 .commandBufferCount = 1,
				 .pCommandBuffers = &command_buffer,
			 },
			 fence));
	VK(vkWaitForFences(device, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
	VK(vkDeviceWaitIdle(device));

	/* The same work given as NIR, compiled by ACO; RADV compiles a clone
	 * with the device's options and leaves the caller's shader as it was. */
	const nir_shader_compiler_options *compute = options[MESA_SHADER_COMPUTE];
	nir_shader *nir = nir_shader_of(compute);
	VkPipelineLayout nir_layout;
	VK(vkCreatePipelineLayout(device,
				  &(VkPipelineLayoutCreateInfo){
					  .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
					  .pushConstantRangeCount = 1,
					  .pPushConstantRanges =
						  &(VkPushConstantRange){
							  .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
							  .size = 8,
						  },
				  },
				  NULL, &nir_layout));
	const VkPipelineShaderStageNirCreateInfoMESA nir_stage = {
		.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_NIR_CREATE_INFO_MESA,
		.nir = nir,
	};
	VkPipeline nir_pipeline;
	VK(vkCreateComputePipelines(
		device, VK_NULL_HANDLE, 1,
		&(VkComputePipelineCreateInfo){
			.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
			.flags = VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR,
			.stage =
				{
					.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
					.pNext = &nir_stage,
					.stage = VK_SHADER_STAGE_COMPUTE_BIT,
					.pName = "main",
				},
			.layout = nir_layout,
		},
		NULL, &nir_pipeline));
	CHECK(nir->options == compute);
	uint64_t nir_instructions, nir_code_bytes;
	aco_code(device, nir_pipeline, vkGetPipelineExecutablePropertiesKHR,
		 vkGetPipelineExecutableStatisticsKHR, &nir_instructions, &nir_code_bytes);
	/* A load, a multiply-add and a store at least: not an empty program. */
	CHECK(nir_instructions >= 3 && nir_code_bytes > 0);
	vkDestroyPipeline(device, nir_pipeline, NULL);
	vkDestroyPipelineLayout(device, nir_layout, NULL);
	ralloc_free(nir);

	vkDestroyFence(device, fence, NULL);
	vkDestroyCommandPool(device, commands, NULL);
	vkDestroyDescriptorPool(device, pool, NULL);
	vkDestroyBuffer(device, buffer, NULL);
	vkFreeMemory(device, allocation, NULL);
	vkDestroyPipeline(device, pipeline, NULL);
	vkDestroyPipelineLayout(device, layout, NULL);
	vkDestroyDescriptorSetLayout(device, set_layout, NULL);
	vkDestroyShaderModule(device, module, NULL);
	vkDestroyDevice(device, NULL);
	vkDestroyInstance(instance, NULL);
	printf("RADV on amdgpu: %s (0x%04x, %s), NIR options for %u stages, compute pipeline of "
	       "%llu instructions in %llu bytes, one dispatch submitted, NIR pipeline of %llu "
	       "instructions in %llu bytes\n",
	       properties.properties.deviceName, properties.properties.deviceID, argv[1], STAGES,
	       (unsigned long long)instructions, (unsigned long long)code_bytes,
	       (unsigned long long)nir_instructions, (unsigned long long)nir_code_bytes);
	printf("amdgpu ioctls:");
	for (unsigned number = 0; number < sizeof(counts) / sizeof(counts[0]); number++) {
		if (!counts[number])
			continue;
		if (number < sizeof(names) / sizeof(names[0]) && names[number])
			printf(" %s=%u", names[number], counts[number]);
		else
			printf(" 0x%02x=%u", number, counts[number]);
	}
	printf("\n");
	return 0;
}
