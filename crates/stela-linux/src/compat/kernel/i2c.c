// SPDX-License-Identifier: GPL-2.0-or-later
/* Parts are adapted from the Linux files named below, whose notices are:
 * Copyright (C) 1995-99 Simon G. Vogl
 * Copyright (C) 2013-2017 Wolfram Sang <wsa@kernel.org>
 */
/* Linux's I2C core: adapters, transfers and client devices, following Linux
 * cc9de5d4b5ab6459f5774946aa3195ad51236fa8 drivers/i2c/i2c-core-base.c. No
 * I2C drivers run here, so a client stays unbound, which is what a Linux
 * caller sees when no driver claims a device. */
#include <linux/completion.h>
#include <linux/device.h>
#include <linux/err.h>
#include <linux/i2c.h>
#include <linux/jiffies.h>
#include <linux/rtmutex.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <stela/linux.h>

static int next_adapter;

/* The bus clients are on, which only names it: no I2C driver matches here. */
const struct bus_type i2c_bus_type = {.name = "i2c"};

/* What an adapter's device is, so i2c_verify_adapter tells it from others. */
const struct device_type i2c_adapter_type = {0};

static void lock_bus(struct i2c_adapter *adapter, unsigned int flags)
{
	(void)flags;
	rt_mutex_lock(&adapter->bus_lock);
}

static int trylock_bus(struct i2c_adapter *adapter, unsigned int flags)
{
	(void)flags;
	return rt_mutex_trylock(&adapter->bus_lock);
}

static void unlock_bus(struct i2c_adapter *adapter, unsigned int flags)
{
	(void)flags;
	rt_mutex_unlock(&adapter->bus_lock);
}

static const struct i2c_lock_operations bus_lock_ops = {
	.lock_bus = lock_bus,
	.trylock_bus = trylock_bus,
	.unlock_bus = unlock_bus,
};

static void adapter_released(struct device *dev)
{
	complete(&to_i2c_adapter(dev)->dev_released);
}

/* Adapter numbers name no fixed board wiring under Stela. */
int i2c_add_numbered_adapter(struct i2c_adapter *adapter)
{
	return i2c_add_adapter(adapter);
}

int i2c_add_adapter(struct i2c_adapter *adapter)
{
	if (WARN(!adapter->name[0], "an I2C adapter has no name\n"))
		return -EINVAL;
	if (WARN(!adapter->algo, "adapter '%s': no algo supplied\n", adapter->name))
		return -EINVAL;
	if (!adapter->lock_ops)
		adapter->lock_ops = &bus_lock_ops;
	rt_mutex_init(&adapter->bus_lock);
	rt_mutex_init(&adapter->mux_lock);
	if (!adapter->timeout)
		adapter->timeout = HZ;
	init_completion(&adapter->dev_released);
	adapter->nr = next_adapter++;
	device_initialize(&adapter->dev);
	adapter->dev.type = &i2c_adapter_type;
	adapter->dev.release = adapter_released;
	return dev_set_name(&adapter->dev, "i2c-%d", adapter->nr);
}

void i2c_del_adapter(struct i2c_adapter *adapter)
{
	put_device(&adapter->dev);
	wait_for_completion(&adapter->dev_released);
}

static void managed_del_adapter(void *adapter)
{
	i2c_del_adapter(adapter);
}

/* An adapter that goes when dev's driver detaches, as in i2c-core-base.c. */
int devm_i2c_add_adapter(struct device *dev, struct i2c_adapter *adapter)
{
	int error = i2c_add_adapter(adapter);

	if (error)
		return error;
	return devm_add_action_or_reset(dev, managed_del_adapter, adapter);
}

/* An adapter by its bus number, which a panel's power sequence in the VBT
 * uses for a platform I2C controller that firmware tables number. Stela
 * drives no such controller and numbers no adapter, so there is none, and
 * that is said once; nothing can then be put. */
struct i2c_adapter *i2c_get_adapter(int nr)
{
	(void)nr;
	stela_linux_unsupported("a platform I2C controller by bus number");
	return NULL;
}

void i2c_put_adapter(struct i2c_adapter *adapter)
{
	if (adapter)
		stela_linux_fatal("an I2C adapter put that no lookup gave");
}

/* An adapter's stated flaws, checked before each transfer as Linux's
 * i2c_check_for_quirks does (i2c-core-base.c). */
#define i2c_quirk_exceeded(val, quirk) ((quirk) && ((val) > (quirk)))

static int i2c_quirk_error(struct i2c_adapter *adapter, struct i2c_msg *msg, const char *err)
{
	dev_err(&adapter->dev, "adapter quirk: %s (addr 0x%04x, size %u, %s)\n", err,
		msg->addr, msg->len, msg->flags & I2C_M_RD ? "read" : "write");
	return -EOPNOTSUPP;
}

static int i2c_check_for_quirks(struct i2c_adapter *adapter, struct i2c_msg *msgs, int num)
{
	const struct i2c_adapter_quirks *q = adapter->quirks;
	int max_num = q->max_num_msgs;
	bool do_len_check = true;

	if (q->flags & I2C_AQ_COMB) {
		max_num = 2;
		if (num == 2) {
			if (q->flags & I2C_AQ_COMB_WRITE_FIRST && msgs[0].flags & I2C_M_RD)
				return i2c_quirk_error(adapter, &msgs[0], "1st comb msg must be write");
			if (q->flags & I2C_AQ_COMB_READ_SECOND && !(msgs[1].flags & I2C_M_RD))
				return i2c_quirk_error(adapter, &msgs[1], "2nd comb msg must be read");
			if (q->flags & I2C_AQ_COMB_SAME_ADDR && msgs[0].addr != msgs[1].addr)
				return i2c_quirk_error(adapter, &msgs[0], "comb msg only to same addr");
			if (i2c_quirk_exceeded(msgs[0].len, q->max_comb_1st_msg_len))
				return i2c_quirk_error(adapter, &msgs[0], "msg too long");
			if (i2c_quirk_exceeded(msgs[1].len, q->max_comb_2nd_msg_len))
				return i2c_quirk_error(adapter, &msgs[1], "msg too long");
			do_len_check = false;
		}
	}
	if (i2c_quirk_exceeded(num, max_num))
		return i2c_quirk_error(adapter, &msgs[0], "too many messages");
	for (int i = 0; i < num; i++) {
		u16 len = msgs[i].len;

		if (msgs[i].flags & I2C_M_RD) {
			if (do_len_check && i2c_quirk_exceeded(len, q->max_read_len))
				return i2c_quirk_error(adapter, &msgs[i], "msg too long");
			if (q->flags & I2C_AQ_NO_ZERO_LEN_READ && len == 0)
				return i2c_quirk_error(adapter, &msgs[i], "no zero length");
		} else {
			if (do_len_check && i2c_quirk_exceeded(len, q->max_write_len))
				return i2c_quirk_error(adapter, &msgs[i], "msg too long");
			if (q->flags & I2C_AQ_NO_ZERO_LEN_WRITE && len == 0)
				return i2c_quirk_error(adapter, &msgs[i], "no zero length");
		}
	}
	return 0;
}

/* Arbitration loss retries, within the adapter's limits. */
int __i2c_transfer(struct i2c_adapter *adapter, struct i2c_msg *msgs, int num)
{
	unsigned long start = jiffies;
	int ret = -EOPNOTSUPP;

	if (!adapter->algo->xfer)
		return ret;
	if (adapter->quirks && i2c_check_for_quirks(adapter, msgs, num))
		return -EOPNOTSUPP;
	for (int attempt = 0; attempt <= adapter->retries; attempt++) {
		ret = adapter->algo->xfer(adapter, msgs, num);
		if (ret != -EAGAIN || time_after(jiffies, start + (unsigned long)adapter->timeout))
			break;
	}
	return ret;
}

int i2c_transfer(struct i2c_adapter *adapter, struct i2c_msg *msgs, int num)
{
	i2c_lock_bus(adapter, I2C_LOCK_SEGMENT);
	int ret = __i2c_transfer(adapter, msgs, num);
	i2c_unlock_bus(adapter, I2C_LOCK_SEGMENT);
	return ret;
}

/* One message to or from a client: the bytes moved, or the error. */
int i2c_transfer_buffer_flags(const struct i2c_client *client, char *buf, int count, u16 flags)
{
	struct i2c_msg msg = {
		.addr = client->addr,
		.flags = flags | (client->flags & I2C_M_TEN),
		.len = (u16)count,
		.buf = (u8 *)buf,
	};
	int ret = i2c_transfer(client->adapter, &msg, 1);

	return ret == 1 ? count : ret;
}

/* The adapter a device is, or NULL for a device of another type. */
struct i2c_adapter *i2c_verify_adapter(struct device *dev)
{
	return dev->type == &i2c_adapter_type ? to_i2c_adapter(dev) : NULL;
}

static void client_released(struct device *dev)
{
	kfree(to_i2c_client(dev));
}

const struct device_type i2c_client_type = {
	.release = client_released,
};

/* The client a device is, or NULL for a device of another type. */
struct i2c_client *i2c_verify_client(struct device *dev)
{
	return dev->type == &i2c_client_type ? to_i2c_client(dev) : NULL;
}

struct i2c_client *i2c_new_client_device(struct i2c_adapter *adapter,
					 const struct i2c_board_info *info)
{
	unsigned short limit = info->flags & I2C_CLIENT_TEN ? 0x3ff : 0x7f;

	if (!info->addr || info->addr > limit)
		return ERR_PTR(-EINVAL);
	struct i2c_client *client = kzalloc_obj(*client);
	if (!client)
		return ERR_PTR(-ENOMEM);
	client->adapter = adapter;
	client->flags = info->flags;
	client->addr = info->addr;
	client->init_irq = info->irq;
	client->irq = info->irq;
	strscpy(client->name, info->type, sizeof(client->name));
	device_initialize(&client->dev);
	client->dev.parent = &adapter->dev;
	client->dev.platform_data = info->platform_data;
	client->dev.type = &i2c_client_type;
	return client;
}

void i2c_unregister_device(struct i2c_client *client)
{
	if (!IS_ERR_OR_NULL(client))
		put_device(&client->dev);
}
