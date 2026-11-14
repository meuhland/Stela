#include "StelaNet.h"

/* The ifnet KPI is the prototype's on purpose: no IOKit vtables to match. */
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

#include <IOKit/IOLib.h>
#include <stela/net.h>

extern "C" {
#include <net/bpf.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <net/if_dl.h>
#include <net/if_ether.h>
#include <net/if_types.h>
#include <sys/errno.h>
#include <sys/socket.h>
#include <sys/sockio.h>
}

OSDefineMetaClassAndStructors(StelaNet, IOService);

/* Entries of each queue: as many frames as the host's queues hold. */
static const uint16_t kQueueEntries = 256;
/* Bytes of each buffer: a header and the longest frame, rounded up. */
static const uint32_t kBufferBytes = 2048;
/* The guest's page: buffers stay inside one, so each is contiguous. */
static const uint32_t kPage = 0x4000;
/* An event index this far ahead of the used index never comes. */
static const uint16_t kNever = 0x8000;
/* virtio_net_hdr_v1 flags. */
static const uint8_t kNeedsCsum = 1;
static const uint8_t kDataValid = 2;

static_assert(STELA_NET_HEADER_BYTES + STELA_NET_FRAME_MAX <= kBufferBytes, "a frame fits a buffer");
static_assert(kPage % kBufferBytes == 0, "no buffer crosses a page");
static_assert(STELA_NET_RING_BYTES(kQueueEntries) <= kPage, "a ring fits a page");
/* The generated layout matches stela_contracts::net::ring's own test. */
static_assert(STELA_NET_RING_USED(256) == 4672, "the used ring sits where the device reads it");

namespace {

struct Descriptor {
	uint64_t address;
	uint32_t length;
	uint16_t flags;
	uint16_t next;
};

struct UsedElement {
	uint32_t id;
	uint32_t length;
};

/* Whether an index moving from `old` to `now` passes `event` (vring_need_event). */
bool
needEvent(uint16_t event, uint16_t now, uint16_t old)
{
	return static_cast<uint16_t>(now - event - 1) < static_cast<uint16_t>(now - old);
}

Descriptor *
descriptors(StelaNet::Queue *queue)
{
	return reinterpret_cast<Descriptor *>(queue->base);
}

uint16_t *
availWord(StelaNet::Queue *queue, uint64_t word)
{
	return reinterpret_cast<uint16_t *>(queue->base + STELA_NET_RING_AVAIL(queue->size) + 2 * word);
}

/* The available ring's idx, ring[i] and used_event. */
uint16_t *availIndex(StelaNet::Queue *queue) { return availWord(queue, 1); }
uint16_t *availSlot(StelaNet::Queue *queue, uint16_t index) { return availWord(queue, 2 + index % queue->size); }
uint16_t *usedEvent(StelaNet::Queue *queue) { return availWord(queue, 2 + queue->size); }

uint8_t *
usedRing(StelaNet::Queue *queue)
{
	return queue->base + STELA_NET_RING_USED(queue->size);
}

uint16_t *usedFlags(StelaNet::Queue *queue) { return reinterpret_cast<uint16_t *>(usedRing(queue)); }
uint16_t *usedIndex(StelaNet::Queue *queue) { return reinterpret_cast<uint16_t *>(usedRing(queue) + 2); }

UsedElement *
usedSlot(StelaNet::Queue *queue, uint16_t index)
{
	return reinterpret_cast<UsedElement *>(usedRing(queue) + 4 + 8 * (index % queue->size));
}

uint16_t *
availEvent(StelaNet::Queue *queue)
{
	return reinterpret_cast<uint16_t *>(usedRing(queue) + 4 + 8 * queue->size);
}

uint8_t *
buffer(StelaNet::Queue *queue, uint16_t slot)
{
	return static_cast<uint8_t *>(queue->buffers->getBytesNoCopy()) + uint64_t(slot) * kBufferBytes;
}

uint64_t
bufferPhysical(StelaNet::Queue *queue, uint16_t slot)
{
	IOByteCount bytes = 0;
	addr64_t physical = queue->buffers->getPhysicalSegment(uint64_t(slot) * kBufferBytes, &bytes,
	    kIOMemoryMapperNone);
	return bytes >= kBufferBytes ? physical : 0;
}

const uint8_t kBroadcast[ETHER_ADDR_LEN] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};

} // namespace

bool
StelaNet::start(IOService *provider)
{
	if (!IOService::start(provider)) {
		return false;
	}
	if (!mapRegisters(provider)) {
		stop(provider);
		return false;
	}
	uint64_t offered = read(STELA_NET_FEATURES);
	if (!(offered & STELA_NET_F_VERSION_1)) {
		IOLog("StelaNet: the device offers no VERSION_1 (%llx)\n", offered);
		stop(provider);
		return false;
	}
	// Checksum offloads are taken when offered: the host completes partial
	// transmit checksums and vouches for received ones.
	accepted = offered & (STELA_NET_F_VERSION_1 | STELA_NET_F_MAC | STELA_NET_F_EVENT_IDX | STELA_NET_F_CSUM |
	    STELA_NET_F_GUEST_CSUM);
	uint64_t station = read(STELA_NET_MAC);
	for (int index = 0; index < ETHER_ADDR_LEN; index++) {
		mac[index] = uint8_t(station >> (8 * index));
	}
	txLock = IOLockAlloc();
	rx.size = kQueueEntries;
	tx.size = kQueueEntries;
	if (!txLock || !allocateQueue(&rx) || !allocateQueue(&tx)) {
		IOLog("StelaNet: cannot allocate the queues\n");
		stop(provider);
		return false;
	}
	// Every receive buffer is posted at once, each the device's to write;
	// transmit slots are filled as packets come. No transmit completion
	// interrupts: output reclaims slots itself.
	for (uint16_t slot = 0; slot < rx.size; slot++) {
		uint64_t physical = bufferPhysical(&rx, slot);
		if (physical == 0) {
			IOLog("StelaNet: a receive buffer is not contiguous\n");
			stop(provider);
			return false;
		}
		descriptors(&rx)[slot] = {physical, kBufferBytes, STELA_NET_DESC_F_WRITE, 0};
		*availSlot(&rx, slot) = slot;
	}
	rx.avail = rx.size;
	__atomic_store_n(availIndex(&rx), rx.avail, __ATOMIC_RELEASE);
	*usedEvent(&rx) = 0;
	*usedEvent(&tx) = kNever;

	workLoop = IOWorkLoop::workLoop();
	interrupt = workLoop
	    ? IOInterruptEventSource::interruptEventSource(this, &StelaNet::interruptOccurred, provider, 0)
	    : nullptr;
	if (!interrupt || workLoop->addEventSource(interrupt) != kIOReturnSuccess) {
		IOLog("StelaNet: cannot take the device's interrupt\n");
		OSSafeReleaseNULL(interrupt);
		stop(provider);
		return false;
	}
	write(STELA_NET_ACCEPT, accepted);
	write(STELA_NET_RX_RING, rx.physical);
	write(STELA_NET_RX_SIZE, rx.size);
	write(STELA_NET_TX_RING, tx.physical);
	write(STELA_NET_TX_SIZE, tx.size);
	write(STELA_NET_FILTER, STELA_NET_FILTER_DIRECTED | STELA_NET_FILTER_BROADCAST | STELA_NET_FILTER_ALL_MULTICAST);
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	write(STELA_NET_ENABLE, 1);
	if (read(STELA_NET_ENABLE) != 1) {
		IOLog("StelaNet: the device refused its rings (cause %llx)\n", read(STELA_NET_CAUSE));
		stop(provider);
		return false;
	}
	IOLockLock(txLock);
	running = true;
	IOLockUnlock(txLock);
	interrupt->enable();
	write(STELA_NET_KICK, STELA_NET_KICK_RX);
	if (!attachInterface()) {
		stop(provider);
		return false;
	}
	IOLog("StelaNet: stn0 %02x:%02x:%02x:%02x:%02x:%02x, features %llx, %u-entry queues\n", mac[0], mac[1],
	    mac[2], mac[3], mac[4], mac[5], accepted, unsigned(kQueueEntries));
	registerService();
	return true;
}

bool
StelaNet::mapRegisters(IOService *provider)
{
	IODeviceMemory *page = provider->getDeviceMemoryWithIndex(0);
	if (!page || page->getLength() != STELA_NET_BYTES) {
		IOLog("StelaNet: the register page is not %llu bytes\n", STELA_NET_BYTES);
		return false;
	}
	registers = page->map(kIOMapInhibitCache);
	if (!registers) {
		IOLog("StelaNet: cannot map the register page\n");
		return false;
	}
	uint64_t version = read(STELA_NET_VERSION);
	if (version != (STELA_NET_MAGIC << 32 | STELA_NET_LAYOUT_VERSION)) {
		IOLog("StelaNet: unknown device version %llx\n", version);
		return false;
	}
	return true;
}

/* A ring in one zeroed guest page, and its buffers in pages of their own. */
bool
StelaNet::allocateQueue(Queue *queue)
{
	// Only a prepared (wired) descriptor is kept, so freeQueue completes
	// each one it finds.
	auto prepared = [](IOBufferMemoryDescriptor *memory) -> IOBufferMemoryDescriptor * {
		if (memory && memory->prepare() != kIOReturnSuccess) {
			memory->release();
			return nullptr;
		}
		return memory;
	};
	queue->ring = prepared(IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage));
	queue->buffers = prepared(IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut,
	    uint64_t(queue->size) * kBufferBytes, kPage));
	if (!queue->ring || !queue->buffers) {
		return false;
	}
	queue->base = static_cast<uint8_t *>(queue->ring->getBytesNoCopy());
	bzero(queue->base, kPage);
	IOByteCount bytes = 0;
	queue->physical = queue->ring->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone);
	return queue->physical != 0 && bytes >= STELA_NET_RING_BYTES(queue->size);
}

void
StelaNet::freeQueue(Queue *queue)
{
	// Only a prepared descriptor is kept.
	if (queue->ring) {
		queue->ring->complete();
		OSSafeReleaseNULL(queue->ring);
	}
	if (queue->buffers) {
		queue->buffers->complete();
		OSSafeReleaseNULL(queue->buffers);
	}
	queue->base = nullptr;
}

bool
StelaNet::attachInterface()
{
	struct ifnet_init_params init = {};
	static const char kUnique[] = "stela-net0";
	init.uniqueid = kUnique;
	init.uniqueid_len = sizeof kUnique;
	init.name = "stn";
	init.unit = 0;
	init.family = IFNET_FAMILY_ETHERNET;
	init.type = IFT_ETHER;
	init.output = &StelaNet::ifOutput;
	init.demux = ether_demux;
	init.add_proto = ether_add_proto;
	init.del_proto = ether_del_proto;
	init.check_multi = ether_check_multi;
	init.framer = ether_frameout;
	init.softc = this;
	init.ioctl = &StelaNet::ifIoctl;
	init.detach = &StelaNet::ifDetached;
	init.broadcast_addr = kBroadcast;
	init.broadcast_len = ETHER_ADDR_LEN;
	errno_t error = ifnet_allocate(&init, &ifp);
	if (error != 0) {
		IOLog("StelaNet: ifnet_allocate: %d\n", error);
		ifp = nullptr;
		return false;
	}
	ifnet_set_mtu(ifp, ETHERMTU);
	ifnet_set_hdrlen(ifp, ETHER_HDR_LEN);
	ifnet_set_baudrate(ifp, STELA_NET_LINK_MBPS * 1000000ULL);
	ifnet_set_flags(ifp, IFF_BROADCAST | IFF_SIMPLEX | IFF_MULTICAST | IFF_RUNNING,
	    IFF_BROADCAST | IFF_SIMPLEX | IFF_MULTICAST | IFF_RUNNING);
	struct sockaddr_dl link = {};
	link.sdl_len = sizeof link;
	link.sdl_family = AF_LINK;
	link.sdl_type = IFT_ETHER;
	link.sdl_alen = ETHER_ADDR_LEN;
	memcpy(link.sdl_data, mac, ETHER_ADDR_LEN);
	// The interface holds this object until it detaches.
	retain();
	error = ifnet_attach(ifp, &link);
	if (error != 0) {
		IOLog("StelaNet: ifnet_attach: %d\n", error);
		ifnet_release(ifp);
		ifp = nullptr;
		release();
		return false;
	}
	bpf_attach(ifp, DLT_EN10MB, ETHER_HDR_LEN, nullptr, nullptr);
	if (accepted & STELA_NET_F_CSUM) {
		// The stack then leaves TCP and UDP checksums partial, the pseudo
		// header's sum in the field, for transmit() to mark.
		error = ifnet_set_offload(ifp, IFNET_CSUM_TCP | IFNET_CSUM_UDP | IFNET_CSUM_TCPIPV6 | IFNET_CSUM_UDPIPV6);
		if (error != 0) {
			IOLog("StelaNet: ifnet_set_offload: %d\n", error);
		}
	}
	__atomic_store_n(&attached, true, __ATOMIC_RELEASE);
	return true;
}

void
StelaNet::stop(IOService *provider)
{
	if (txLock) {
		IOLockLock(txLock);
		running = false;
		IOLockUnlock(txLock);
	}
	if (registers) {
		// Once this store retires the device reaches no guest memory.
		write(STELA_NET_ENABLE, 0);
	}
	if (interrupt) {
		interrupt->disable();
		workLoop->removeEventSource(interrupt);
		OSSafeReleaseNULL(interrupt);
	}
	if (ifp && __atomic_exchange_n(&attached, false, __ATOMIC_ACQ_REL)) {
		// ifDetached releases the interface and this object.
		ifnet_detach(ifp);
	}
	IOService::stop(provider);
}

void
StelaNet::free()
{
	freeQueue(&rx);
	freeQueue(&tx);
	OSSafeReleaseNULL(workLoop);
	OSSafeReleaseNULL(registers);
	if (txLock) {
		IOLockFree(txLock);
		txLock = nullptr;
	}
	IOService::free();
}

void
StelaNet::ifDetached(ifnet_t interface)
{
	auto net = static_cast<StelaNet *>(ifnet_softc(interface));
	net->ifp = nullptr;
	ifnet_release(interface);
	net->release();
}

errno_t
StelaNet::ifOutput(ifnet_t interface, mbuf_t packet)
{
	return static_cast<StelaNet *>(ifnet_softc(interface))->transmit(packet);
}

void
StelaNet::reclaimTransmit()
{
	uint16_t done = __atomic_load_n(usedIndex(&tx), __ATOMIC_ACQUIRE);
	// The device completes transmit chains in order, one slot each.
	txInFlight -= static_cast<uint16_t>(done - tx.used);
	tx.used = done;
	*usedEvent(&tx) = static_cast<uint16_t>(tx.used + kNever);
}

/*
 * Copies the packet after a zeroed header into the next transmit slot,
 * publishes it and kicks when the device asked to hear of it. A full ring
 * drops the packet: the prototype has no output queue of its own.
 */
errno_t
StelaNet::transmit(mbuf_t packet)
{
	size_t length = mbuf_pkthdr_len(packet);
	bool sent = false;
	IOLockLock(txLock);
	uint16_t old = tx.avail;
	if (running) {
		reclaimTransmit();
		if (txInFlight < tx.size && length >= ETHER_HDR_LEN && length <= STELA_NET_FRAME_MAX) {
			uint16_t slot = tx.avail % tx.size;
			uint8_t *data = buffer(&tx, slot);
			bzero(data, STELA_NET_HEADER_BYTES);
			if (mbuf_copydata(packet, 0, length, data + STELA_NET_HEADER_BYTES) == 0) {
				markPartialChecksum(packet, data, length);
				uint64_t physical = bufferPhysical(&tx, slot);
				descriptors(&tx)[slot] = {physical, uint32_t(STELA_NET_HEADER_BYTES + length), 0, 0};
				*availSlot(&tx, tx.avail) = slot;
				tx.avail++;
				txInFlight++;
				__atomic_store_n(availIndex(&tx), tx.avail, __ATOMIC_RELEASE);
				sent = true;
			}
		}
	}
	if (!sent) {
		txDropped++;
	}
	IOLockUnlock(txLock);
	if (sent) {
		kick(&tx, old, STELA_NET_KICK_TX);
		bpf_tap_out(ifp, DLT_EN10MB, packet, nullptr, 0);
		ifnet_stat_increment_out(ifp, 1, uint32_t(length), 0);
	} else {
		ifnet_stat_increment_out(ifp, 0, 0, 1);
	}
	mbuf_freem(packet);
	return sent ? 0 : ENOBUFS;
}

/*
 * Marks a packet whose TCP or UDP checksum the stack left partial
 * (`mbuf_get_csum_requested`) NEEDS_CSUM in its virtio_net_hdr, from the
 * transport header to the end, the field at its offset in that header.
 */
void
StelaNet::markPartialChecksum(mbuf_t packet, uint8_t *data, size_t length)
{
	if (!(accepted & STELA_NET_F_CSUM)) {
		return;
	}
	mbuf_csum_request_flags_t request = 0;
	u_int32_t value = 0;
	if (mbuf_get_csum_requested(packet, &request, &value) != 0) {
		return;
	}
	bool tcp = request & (MBUF_CSUM_REQ_TCP | MBUF_CSUM_REQ_TCPIPV6);
	bool udp = request & (MBUF_CSUM_REQ_UDP | MBUF_CSUM_REQ_UDPIPV6);
	if (!tcp && !udp) {
		return;
	}
	const uint8_t *frame = data + STELA_NET_HEADER_BYTES;
	size_t network;
	if (request & (MBUF_CSUM_REQ_TCPIPV6 | MBUF_CSUM_REQ_UDPIPV6)) {
		network = 40;
	} else {
		network = size_t(frame[ETHER_HDR_LEN] & 0xf) * 4;
	}
	uint16_t start = uint16_t(ETHER_HDR_LEN + network);
	uint16_t offset = tcp ? 16 : 6;
	if (size_t(start) + offset + 2 > length) {
		return;
	}
	data[0] = kNeedsCsum;
	memcpy(data + 6, &start, sizeof start);
	memcpy(data + 8, &offset, sizeof offset);
}

void
StelaNet::kick(Queue *queue, uint16_t old, uint64_t which)
{
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	bool wanted = (accepted & STELA_NET_F_EVENT_IDX)
	    ? needEvent(__atomic_load_n(availEvent(queue), __ATOMIC_ACQUIRE), queue->avail, old)
	    : !(__atomic_load_n(usedFlags(queue), __ATOMIC_ACQUIRE) & STELA_NET_USED_F_NO_NOTIFY);
	if (wanted) {
		write(STELA_NET_KICK, which);
	}
}

/*
 * Every frame the device wrote goes up the stack in one chain, each buffer
 * posted again; then the next used entry is asked to interrupt, and the ring
 * looked at once more for frames that came meanwhile.
 */
void
StelaNet::harvestReceive()
{
	for (;;) {
		uint16_t done = __atomic_load_n(usedIndex(&rx), __ATOMIC_ACQUIRE);
		if (done == rx.used) {
			return;
		}
		uint16_t old = rx.avail;
		bool deliver = __atomic_load_n(&attached, __ATOMIC_ACQUIRE);
		mbuf_t first = nullptr;
		mbuf_t last = nullptr;
		uint32_t packets = 0;
		uint32_t bytes = 0;
		while (rx.used != done) {
			UsedElement element = *usedSlot(&rx, rx.used);
			rx.used++;
			uint16_t slot = uint16_t(element.id);
			if (slot >= rx.size) {
				continue;
			}
			uint32_t length = element.length - STELA_NET_HEADER_BYTES;
			mbuf_t packet = nullptr;
			if (deliver && element.length > STELA_NET_HEADER_BYTES + ETHER_HDR_LEN &&
			    element.length <= kBufferBytes && mbuf_getpacket(MBUF_DONTWAIT, &packet) == 0) {
				const uint8_t *data = buffer(&rx, slot);
				memcpy(mbuf_data(packet), data + STELA_NET_HEADER_BYTES, length);
				if ((accepted & STELA_NET_F_GUEST_CSUM) && (data[0] & kDataValid)) {
					// The host verified the TCP or UDP checksum, and an IPv4
					// header's, so the stack skips them.
					bool ipv4 = data[STELA_NET_HEADER_BYTES + 12] == 0x08 &&
					    data[STELA_NET_HEADER_BYTES + 13] == 0x00;
					mbuf_set_csum_performed(packet,
					    MBUF_CSUM_DID_DATA | MBUF_CSUM_PSEUDO_HDR |
					        (ipv4 ? MBUF_CSUM_DID_IP | MBUF_CSUM_IP_GOOD : 0),
					    0xffff);
				}
				mbuf_setlen(packet, length);
				mbuf_pkthdr_setlen(packet, length);
				mbuf_pkthdr_setrcvif(packet, ifp);
				bpf_tap_in(ifp, DLT_EN10MB, packet, nullptr, 0);
				// The stack takes the Ethernet header by the packet
				// header's pointer and the payload after it.
				mbuf_pkthdr_setheader(packet, mbuf_data(packet));
				mbuf_adj(packet, ETHER_HDR_LEN);
				if (last) {
					mbuf_setnextpkt(last, packet);
				} else {
					first = packet;
				}
				last = packet;
				packets++;
				bytes += length;
			} else {
				rxDropped++;
			}
			*availSlot(&rx, rx.avail) = slot;
			rx.avail++;
		}
		__atomic_store_n(availIndex(&rx), rx.avail, __ATOMIC_RELEASE);
		*usedEvent(&rx) = rx.used;
		kick(&rx, old, STELA_NET_KICK_RX);
		if (first) {
			struct ifnet_stat_increment_param stats = {};
			stats.packets_in = packets;
			stats.bytes_in = bytes;
			ifnet_input(ifp, first, &stats);
		}
	}
}

/*
 * The device holds its level line until every cause is acknowledged;
 * acknowledging before harvesting means frames arriving meanwhile raise it
 * again.
 */
void
StelaNet::interruptOccurred(OSObject *owner, IOInterruptEventSource *, int)
{
	auto net = static_cast<StelaNet *>(owner);
	uint64_t cause = net->read(STELA_NET_CAUSE);
	net->write(STELA_NET_ACK, cause);
	if (cause & STELA_NET_CAUSE_ERROR) {
		IOLog("StelaNet: the device stopped on an error\n");
		IOLockLock(net->txLock);
		net->running = false;
		IOLockUnlock(net->txLock);
		return;
	}
	if (cause & STELA_NET_CAUSE_RX) {
		net->harvestReceive();
	}
	if (cause & STELA_NET_CAUSE_TX) {
		IOLockLock(net->txLock);
		net->reclaimTransmit();
		IOLockUnlock(net->txLock);
	}
}

errno_t
StelaNet::ifIoctl(ifnet_t interface, unsigned long command, void *data)
{
	auto net = static_cast<StelaNet *>(ifnet_softc(interface));
	switch (command) {
	case SIOCSIFFLAGS: {
		uint64_t filter = STELA_NET_FILTER_DIRECTED | STELA_NET_FILTER_BROADCAST | STELA_NET_FILTER_ALL_MULTICAST;
		if (ifnet_flags(interface) & IFF_PROMISC) {
			filter |= STELA_NET_FILTER_PROMISCUOUS;
		}
		net->write(STELA_NET_FILTER, filter);
		return 0;
	}
	case SIOCADDMULTI:
	case SIOCDELMULTI:
		// Every multicast group passes the device's filter.
		return 0;
	case SIOCSIFMTU: {
		auto request = static_cast<struct ifreq *>(data);
		if (request->ifr_mtu < 68 || request->ifr_mtu > ETHERMTU) {
			return EINVAL;
		}
		return ifnet_set_mtu(interface, request->ifr_mtu);
	}
	default:
		return ether_ioctl(interface, uint32_t(command), data);
	}
}

uint64_t
StelaNet::read(uint64_t reg) const
{
	return *reinterpret_cast<volatile uint64_t *>(registers->getVirtualAddress() + reg);
}

void
StelaNet::write(uint64_t reg, uint64_t value)
{
	*reinterpret_cast<volatile uint64_t *>(registers->getVirtualAddress() + reg) = value;
}
