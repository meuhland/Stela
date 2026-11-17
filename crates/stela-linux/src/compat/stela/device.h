/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_DEVICE_H
#define STELA_DEVICE_H
struct device;

/* Undoes the managed resources on dev, last first: a driver's teardown. */
void stela_linux_devres_release(struct device *dev);
/* drivers/base/devres.c, which the driver core calls (drivers/base/base.h). */
int devres_release_all(struct device *dev);
#endif
