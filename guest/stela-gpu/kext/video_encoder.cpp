/*
 * Keeps StelaVT, the x264-backed H.264 encoder the Metal plugin registers
 * (bundle/VideoEncoder.m), installed as this build's: its code is in the
 * boot's trust cache, so platform processes such as the video encoder
 * service may map it, and only this exact executable matches that list. A
 * kext creates no directories: without the bundle's directories, the install
 * is reported and macOS keeps its software encoders.
 */
#include "files.h"
#include "video_encoder.h"

#include <IOKit/IOLib.h>
#include <stela/stelavt.h>
#include <sys/errno.h>

#if STELAVT_PRESENT

static int
stage(const char *path, const unsigned char *bytes, unsigned long length, const unsigned char sha256[32], unsigned mode,
    vfs_context_t context, char *buffer, bool *written)
{
	bool same = false;
	int error = stelaMatches(path, sha256, length, context, buffer, &same);
	if (same) {
		return 0;
	}
	if (error && error != ENOENT) {
		return error;
	}
	unsigned step = 0;
	error = stelaWriteFile(path, bytes, length, mode, context, &step);
	if (!error) {
		error = stelaMatches(path, sha256, length, context, buffer, &same);
	}
	*written = true;
	return !error && !same ? EIO : error;
}

void
stelaDeliverVideoEncoder(vfs_context_t context, char *buffer)
{
	bool written = false;
	int error = stage(STELAVT_INFO_PATH, stelavt_info, sizeof stelavt_info, stelavt_info_sha256, 0644, context, buffer,
	    &written);
	if (!error) {
		error = stage(STELAVT_EXECUTABLE_PATH, stelavt_executable, sizeof stelavt_executable,
		    stelavt_executable_sha256, 0755, context, buffer, &written);
	}
	if (error == ENOENT) {
		IOLog("StelaGPU: StelaVT is not installed: /Library/Video/Plug-Ins/StelaVT.bundle/Contents/MacOS "
		      "does not exist\n");
	} else if (error) {
		IOLog("StelaGPU: installing StelaVT failed: %d\n", error);
	} else if (written) {
		IOLog("StelaGPU: installed StelaVT from stela.efi\n");
	}
}

#else

void
stelaDeliverVideoEncoder(vfs_context_t, char *)
{
}

#endif
