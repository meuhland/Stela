/*
 * StelaVT's delivery (video_encoder.cpp): the prebuilt H.264 encoder the
 * Metal plugin registers in processes with VideoToolbox.
 */
#pragma once

#include <sys/vnode.h>

/* Installs StelaVT unless it is this build's; reports what it could not. */
void stelaDeliverVideoEncoder(vfs_context_t context, char *buffer);
