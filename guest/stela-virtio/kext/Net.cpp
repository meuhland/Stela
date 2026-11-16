#include "Net.h"

// The mbuf KPI is deprecated for network kernel extensions, not withdrawn:
// IONetworkingFamily drivers and StelaNet's front end use it.
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

#include <IOKit/IOLib.h>
#include <libkern/c++/OSString.h>
#include <stela/virtio.h>

OSDefineMetaClassAndStructors(StelaVirtIONet, IOEthernetController);

namespace {
/* The header every buffer starts with, and the frames it carries. */
const uint32_t kHeader = STELA_VIRTIO_NET_HEADER_BYTES;
const uint32_t kFrameMin = STELA_VIRTIO_NET_FRAME_MIN;
const uint32_t kFrameMax = STELA_VIRTIO_NET_FRAME_MAX;
/* What the driver accepts of what the device offers. */
const uint64_t kWanted = STELA_VIRTIO_F_VERSION_1 | STELA_VIRTIO_F_EVENT_IDX | STELA_VIRTIO_NET_F_MAC |
    STELA_VIRTIO_NET_F_STATUS | STELA_VIRTIO_NET_F_CSUM | STELA_VIRTIO_NET_F_GUEST_CSUM;
/* The medium it reports: 1000BASE-T, full duplex. */
const UInt64 kSpeed = 1000ULL * 1000 * 1000;
const IOMediumType kMedium = kIOMediumEthernet1000BaseT | kIOMediumOptionFullDuplex;
/* Packets the output queue holds while the ring is full. */
const UInt32 kOutputCapacity = 256;
/* How often the counters are published. */
const uint32_t kCountersMS = 2000;
const size_t kEthernetHeader = 14;
} // namespace

static_assert(StelaVirtIONet::kBufferBytes >= kHeader + kFrameMax, "a frame and its header fit one buffer");
static_assert(StelaVirtIONet::kRxEntries <= STELA_VQ_MAX && StelaVirtIONet::kTxEntries <= STELA_VQ_MAX,
    "the ring core holds every slot");
static_assert(StelaVirtIONet::kRxEntries % StelaVirtIONet::kPerPage == 0, "whole pages of buffers");
static_assert(STELA_VIRTIO_NET_RX_QUEUE == 0 && STELA_VIRTIO_NET_TX_QUEUE == 1, "virtio-net's queue numbers");

uint8_t *
StelaVirtIONet::at(const Buffers *buffers, uint16_t slot)
{
	return buffers->bytes[slot / kPerPage] + (slot % kPerPage) * kBufferBytes;
}

uint64_t
StelaVirtIONet::physicalAt(const Buffers *buffers, uint16_t slot)
{
	return buffers->physical[slot / kPerPage] + (slot % kPerPage) * kBufferBytes;
}

bool
StelaVirtIONet::allocate(Buffers *buffers)
{
	for (unsigned page = 0; page < kRxEntries / kPerPage; page++) {
		auto memory = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage);
		if (memory && memory->prepare() != kIOReturnSuccess) {
			OSSafeReleaseNULL(memory);
		}
		if (!memory) {
			return false;
		}
		buffers->pages[page] = memory;
		IOByteCount bytes = 0;
		buffers->physical[page] = memory->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone);
		buffers->bytes[page] = static_cast<uint8_t *>(memory->getBytesNoCopy());
		if (buffers->physical[page] == 0 || bytes < kPage) {
			return false;
		}
		bzero(buffers->bytes[page], kPage);
	}
	return true;
}

void
StelaVirtIONet::release(Buffers *buffers)
{
	for (auto &page : buffers->pages) {
		if (page) {
			page->complete();
			OSSafeReleaseNULL(page);
		}
	}
}

bool
StelaVirtIONet::readConfig()
{
	uint32_t words[2] = {};
	if (!nub->readConfig(STELA_VIRTIO_NET_CONFIG_MAC, words, 2)) {
		return false;
	}
	static_assert(STELA_VIRTIO_NET_CONFIG_MAC == 0 && STELA_VIRTIO_NET_CONFIG_STATUS == 6,
	    "the address in the first six bytes, the status in the two after");
	memcpy(mac, words, sizeof mac);
	uint16_t status = uint16_t(words[1] >> 16);
	// Without STATUS the link is always up.
	linkUp = !(features & STELA_VIRTIO_NET_F_STATUS) || (status & STELA_VIRTIO_NET_STATUS_LINK_UP);
	return true;
}

bool
StelaVirtIONet::start(IOService *provider)
{
	nub = OSDynamicCast(StelaVirtIODevice, provider);
	if (!nub) {
		return false;
	}
	features = nub->negotiate(kWanted);
	if (features == 0) {
		return false;
	}
	// The address before the family's start, which publishes it.
	if (!(features & STELA_VIRTIO_NET_F_MAC) || !readConfig()) {
		IOLog("StelaVirtIONet: the device gives no station address\n");
		nub->fail();
		return false;
	}
	if (!IOEthernetController::start(provider)) {
		nub->fail();
		return false;
	}
	if (!allocate(&rx) || !allocate(&tx)) {
		IOLog("StelaVirtIONet: cannot allocate its buffers\n");
		nub->fail();
		stop(provider);
		return false;
	}
	if (!nub->createQueue(STELA_VIRTIO_NET_RX_QUEUE, kRxEntries, features, &rxQueue) ||
	    !nub->createQueue(STELA_VIRTIO_NET_TX_QUEUE, kTxEntries, features, &txQueue)) {
		nub->fail();
		stop(provider);
		return false;
	}
	IOWorkLoop *workLoop = getWorkLoop();
	timer = workLoop ? IOTimerEventSource::timerEventSource(this, &StelaVirtIONet::timerFired) : nullptr;
	if (!timer || workLoop->addEventSource(timer) != kIOReturnSuccess) {
		IOLog("StelaVirtIONet: cannot make its timer\n");
		OSSafeReleaseNULL(timer);
		nub->fail();
		stop(provider);
		return false;
	}
	OSDictionary *media = OSDictionary::withCapacity(1);
	medium = IONetworkMedium::medium(kMedium, kSpeed);
	if (!media || !medium || !IONetworkMedium::addMedium(media, medium) || !publishMediumDictionary(media)) {
		IOLog("StelaVirtIONet: cannot publish its medium\n");
		OSSafeReleaseNULL(media);
		nub->fail();
		stop(provider);
		return false;
	}
	OSSafeReleaseNULL(media);
	setSelectedMedium(medium);
	nub->setHandler(this, &StelaVirtIONet::interruptOccurred);
	nub->driverOK();
	workLoop->runAction(&StelaVirtIONet::begin, this);
	publishLink();
	if (!attachInterface(&netif, true)) {
		IOLog("StelaVirtIONet: cannot attach its interface\n");
		nub->fail();
		stop(provider);
		return false;
	}
	timer->setTimeoutMS(kCountersMS);
	IOLog("StelaVirtIONet: %02x:%02x:%02x:%02x:%02x:%02x, features %llx, %u receive buffers, "
	      "%u transmit slots, link %s\n",
	    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], features, unsigned(rxQueue.vq.size),
	    unsigned(txQueue.vq.size), linkUp ? "up" : "down");
	setProperty("VirtIOFeatures", features, 64);
	publishCounters();
	return true;
}

/* On the work loop: the device runs; every receive buffer goes to it. */
IOReturn
StelaVirtIONet::begin(OSObject *target, void *, void *, void *, void *)
{
	auto net = static_cast<StelaVirtIONet *>(target);
	net->running = true;
	// Receive completions interrupt; transmit ones only while stalled.
	stela_vq_arm(&net->rxQueue.vq);
	stela_vq_disarm(&net->txQueue.vq);
	if (net->postReceive()) {
		net->nub->kick(&net->rxQueue);
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIONet::halt(OSObject *target, void *, void *, void *, void *)
{
	auto net = static_cast<StelaVirtIONet *>(target);
	net->running = false;
	net->enabled = false;
	return kIOReturnSuccess;
}

unsigned
StelaVirtIONet::postReceive()
{
	unsigned posted = 0;
	while (rxQueue.vq.free_count > 0) {
		// One descriptor per chain: the head is the slot.
		uint16_t slot = rxQueue.vq.free_head;
		struct stela_vq_buf buffer = {physicalAt(&rx, slot), kBufferBytes};
		if (stela_vq_add(&rxQueue.vq, &buffer, 0, 1, &rxTokens[slot]) != int(slot)) {
			break;
		}
		posted++;
	}
	return posted;
}

void
StelaVirtIONet::failed(const char *why)
{
	IOLog("StelaVirtIONet: %s; the device is left failed\n", why);
	running = false;
	nub->fail();
	linkUp = false;
	publishLink();
}

mbuf_t
StelaVirtIONet::receivePacket(const uint8_t *buffer, uint32_t written)
{
	size_t length = written - kHeader;
	mbuf_t packet = nullptr;
	if (mbuf_getpacket(MBUF_DONTWAIT, &packet) != 0) {
		return nullptr;
	}
	if (mbuf_maxlen(packet) < length) {
		mbuf_freem(packet);
		return nullptr;
	}
	memcpy(mbuf_data(packet), buffer + kHeader, length);
	mbuf_setlen(packet, length);
	mbuf_pkthdr_setlen(packet, length);
	if ((features & STELA_VIRTIO_NET_F_GUEST_CSUM) && (buffer[STELA_VIRTIO_NET_HDR_FLAGS] & STELA_VIRTIO_NET_HDR_F_DATA_VALID)) {
		// The device verified the TCP or UDP checksum, and an IPv4 header's.
		const uint8_t *frame = buffer + kHeader;
		bool ipv4 = frame[12] == 0x08 && frame[13] == 0x00;
		mbuf_set_csum_performed(packet,
		    MBUF_CSUM_DID_DATA | MBUF_CSUM_PSEUDO_HDR | (ipv4 ? MBUF_CSUM_DID_IP | MBUF_CSUM_IP_GOOD : 0), 0xffff);
	}
	return packet;
}

/*
 * Every frame the device wrote goes up the stack, each buffer posted again;
 * then the next used entry is asked to interrupt, and the ring looked at once
 * more for frames that came meanwhile.
 */
void
StelaVirtIONet::receive()
{
	unsigned reposted = 0;
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&rxQueue.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			stela_vq_arm(&rxQueue.vq);
			if (!stela_vq_pending(&rxQueue.vq)) {
				break;
			}
			continue;
		}
		size_t slot = found == STELA_VQ_USED ? size_t(static_cast<uint8_t *>(token) - rxTokens) : kRxEntries;
		if (slot >= kRxEntries) {
			failed("the device used a receive buffer it was not given");
			return;
		}
		mbuf_t packet = nullptr;
		if (written >= kHeader + kFrameMin && written <= kHeader + kFrameMax && enabled && netif) {
			packet = receivePacket(at(&rx, uint16_t(slot)), written);
		}
		if (packet) {
			rxFrames++;
			rxBytes += written - kHeader;
			if (netStats) {
				netStats->inputPackets++;
			}
			netif->inputPacket(packet, uint32_t(written - kHeader), 0, nullptr);
		} else {
			rxDropped++;
			if (netStats && enabled) {
				netStats->inputErrors++;
			}
		}
		reposted += postReceive();
	}
	if (reposted) {
		nub->kick(&rxQueue);
	}
}

void
StelaVirtIONet::reclaimTransmit()
{
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&txQueue.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			return;
		}
		size_t slot = found == STELA_VQ_USED ? size_t(static_cast<uint8_t *>(token) - txTokens) : kTxEntries;
		if (slot >= kTxEntries) {
			failed("the device used a transmit slot it was not given");
			return;
		}
	}
}

/* On the work loop: the transport acknowledged `status`. */
void
StelaVirtIONet::interruptOccurred(OSObject *owner, uint32_t status)
{
	auto net = static_cast<StelaVirtIONet *>(owner);
	net->interrupts++;
	if (status & STELA_VIRTIO_MMIO_INT_CONFIG) {
		bool was = net->linkUp;
		if (net->readConfig() && was != net->linkUp) {
			net->linkChanges++;
			net->publishLink();
		}
	}
	if (!net->running) {
		return;
	}
	if (status & STELA_VIRTIO_MMIO_INT_VRING) {
		net->receive();
		net->reclaimTransmit();
		if (net->stalled && net->running && net->txQueue.vq.free_count > 0) {
			net->stalled = false;
			stela_vq_disarm(&net->txQueue.vq);
			if (net->outputQueue) {
				net->outputQueue->service(kServiceAsync);
			}
		}
	}
}

void
StelaVirtIONet::publishLink()
{
	if (linkUp && running) {
		setLinkStatus(kIONetworkLinkValid | kIONetworkLinkActive, medium, kSpeed);
	} else {
		setLinkStatus(kIONetworkLinkValid);
	}
}

/*
 * Marks a packet whose TCP or UDP checksum the stack left partial
 * (mbuf_get_csum_requested) NEEDS_CSUM in its virtio_net_hdr, from the
 * transport header to the end, the field at its offset in that header, as
 * StelaNet's front end does; an IPv4 header checksum left to the driver is
 * done here.
 */
void
StelaVirtIONet::markChecksums(mbuf_t packet, uint8_t *buffer, size_t length)
{
	mbuf_csum_request_flags_t request = 0;
	u_int32_t value = 0;
	if (mbuf_get_csum_requested(packet, &request, &value) != 0 || request == 0) {
		return;
	}
	uint8_t *frame = buffer + kHeader;
	bool ipv4 = length >= kEthernetHeader + 20 && frame[12] == 0x08 && frame[13] == 0x00;
	size_t network = ipv4 ? size_t(frame[kEthernetHeader] & 0xf) * 4 : 40;
	if ((request & MBUF_CSUM_REQ_IP) && ipv4 && network >= 20 && kEthernetHeader + network <= length) {
		uint8_t *ip = frame + kEthernetHeader;
		ip[10] = ip[11] = 0;
		uint32_t sum = 0;
		for (size_t at = 0; at < network; at += 2) {
			sum += uint32_t(ip[at]) << 8 | ip[at + 1];
		}
		while (sum >> 16) {
			sum = (sum & 0xffff) + (sum >> 16);
		}
		uint16_t checksum = uint16_t(~sum);
		ip[10] = uint8_t(checksum >> 8);
		ip[11] = uint8_t(checksum);
	}
	if (!(features & STELA_VIRTIO_NET_F_CSUM)) {
		return;
	}
	bool tcp = request & (MBUF_CSUM_REQ_TCP | MBUF_CSUM_REQ_TCPIPV6);
	bool udp = request & (MBUF_CSUM_REQ_UDP | MBUF_CSUM_REQ_UDPIPV6);
	if (!tcp && !udp) {
		return;
	}
	if (request & (MBUF_CSUM_REQ_TCPIPV6 | MBUF_CSUM_REQ_UDPIPV6)) {
		network = 40;
	}
	uint16_t start = uint16_t(kEthernetHeader + network);
	uint16_t offset = tcp ? 16 : 6;
	if (size_t(start) + offset + 2 > length) {
		return;
	}
	buffer[STELA_VIRTIO_NET_HDR_FLAGS] = STELA_VIRTIO_NET_HDR_F_NEEDS_CSUM;
	memcpy(buffer + STELA_VIRTIO_NET_HDR_CSUM_START, &start, sizeof start);
	memcpy(buffer + STELA_VIRTIO_NET_HDR_CSUM_OFFSET, &offset, sizeof offset);
}

/* In the work loop's gate, from the gated output queue. */
UInt32
StelaVirtIONet::outputPacket(mbuf_t packet, void *)
{
	if (!running || !enabled) {
		txDropped++;
		freePacket(packet);
		return kIOReturnOutputDropped;
	}
	reclaimTransmit();
	if (txQueue.vq.free_count == 0) {
		// Hear of the next completion, and look again in case it came
		// before the device could see the event.
		stela_vq_arm(&txQueue.vq);
		reclaimTransmit();
		if (txQueue.vq.free_count == 0) {
			stalled = true;
			txStalls++;
			return kIOReturnOutputStall;
		}
		stela_vq_disarm(&txQueue.vq);
	}
	size_t length = mbuf_pkthdr_len(packet);
	if (length < kEthernetHeader || length > kFrameMax) {
		txDropped++;
		if (netStats) {
			netStats->outputErrors++;
		}
		freePacket(packet);
		return kIOReturnOutputDropped;
	}
	// One descriptor per chain: the head is the slot.
	uint16_t slot = txQueue.vq.free_head;
	uint8_t *buffer = at(&tx, slot);
	bzero(buffer, kHeader);
	if (mbuf_copydata(packet, 0, length, buffer + kHeader) != 0) {
		txDropped++;
		freePacket(packet);
		return kIOReturnOutputDropped;
	}
	markChecksums(packet, buffer, length);
	struct stela_vq_buf chain = {physicalAt(&tx, slot), uint32_t(kHeader + length)};
	if (stela_vq_add(&txQueue.vq, &chain, 1, 0, &txTokens[slot]) != int(slot)) {
		txDropped++;
		freePacket(packet);
		return kIOReturnOutputDropped;
	}
	nub->kick(&txQueue);
	txFrames++;
	txBytes += length;
	if (netStats) {
		netStats->outputPackets++;
	}
	freePacket(packet);
	return kIOReturnOutputSuccess;
}

IOOutputQueue *
StelaVirtIONet::createOutputQueue()
{
	outputQueue = IOGatedOutputQueue::withTarget(this, getWorkLoop(), kOutputCapacity);
	return outputQueue;
}

bool
StelaVirtIONet::configureInterface(IONetworkInterface *interface)
{
	if (!IOEthernetController::configureInterface(interface)) {
		return false;
	}
	// The family's statistics buffer, used only when it is as large as the
	// structure this kext knows.
	IONetworkData *data = interface->getNetworkData(kIONetworkStatsKey);
	if (data && data->getSize() >= sizeof(IONetworkStats)) {
		netStats = static_cast<IONetworkStats *>(const_cast<void *>(data->getBuffer()));
	}
	return true;
}

IOReturn
StelaVirtIONet::enable(IONetworkInterface *)
{
	enabled = true;
	if (outputQueue) {
		outputQueue->setCapacity(kOutputCapacity);
		outputQueue->start();
	}
	publishLink();
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIONet::disable(IONetworkInterface *)
{
	enabled = false;
	if (outputQueue) {
		outputQueue->stop();
		outputQueue->setCapacity(0);
		outputQueue->flush();
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIONet::getHardwareAddress(IOEthernetAddress *address)
{
	memcpy(address->bytes, mac, sizeof mac);
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIONet::getChecksumSupport(UInt32 *checksumMask, UInt32 checksumFamily, bool isOutput)
{
	if (checksumFamily != kChecksumFamilyTCPIP) {
		return kIOReturnUnsupported;
	}
	UInt32 mask = 0;
	if (isOutput) {
		if (features & STELA_VIRTIO_NET_F_CSUM) {
			mask = kChecksumTCP | kChecksumUDP | kChecksumTCPIPv6 | kChecksumUDPIPv6;
		}
	} else if (features & STELA_VIRTIO_NET_F_GUEST_CSUM) {
		mask = kChecksumIP | kChecksumTCP | kChecksumUDP;
	}
	*checksumMask = mask;
	return kIOReturnSuccess;
}

/* The device takes every multicast frame; there is nothing to set. */
IOReturn
StelaVirtIONet::setMulticastMode(bool)
{
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIONet::setMulticastList(IOEthernetAddress *, UInt32)
{
	return kIOReturnSuccess;
}

/* The host gives the device its own frames and every group frame only. */
IOReturn
StelaVirtIONet::setPromiscuousMode(bool)
{
	return kIOReturnSuccess;
}

const OSString *
StelaVirtIONet::newVendorString() const
{
	return OSString::withCString("Stela");
}

const OSString *
StelaVirtIONet::newModelString() const
{
	return OSString::withCString("virtio-net");
}

void
StelaVirtIONet::publishCounters()
{
	setProperty("RxFrames", rxFrames, 64);
	setProperty("RxBytes", rxBytes, 64);
	setProperty("RxDropped", rxDropped, 64);
	setProperty("TxFrames", txFrames, 64);
	setProperty("TxBytes", txBytes, 64);
	setProperty("TxDropped", txDropped, 64);
	setProperty("TxStalls", txStalls, 64);
	setProperty("LinkChanges", linkChanges, 64);
	setProperty("Interrupts", interrupts, 64);
	setProperty("LinkUp", linkUp && running);
}

void
StelaVirtIONet::timerFired(OSObject *owner, IOTimerEventSource *timer)
{
	static_cast<StelaVirtIONet *>(owner)->publishCounters();
	timer->setTimeoutMS(kCountersMS);
}

void
StelaVirtIONet::stop(IOService *provider)
{
	IOWorkLoop *workLoop = getWorkLoop();
	if (workLoop) {
		workLoop->runAction(&StelaVirtIONet::halt, this);
	}
	if (netif) {
		detachInterface(netif, false);
		OSSafeReleaseNULL(netif);
	}
	if (nub) {
		nub->setHandler(nullptr, nullptr);
		// Once the reset reads back the device uses no buffer.
		nub->reset();
	}
	if (timer) {
		timer->cancelTimeout();
		workLoop->removeEventSource(timer);
		OSSafeReleaseNULL(timer);
	}
	IOEthernetController::stop(provider);
}

void
StelaVirtIONet::free()
{
	// The device was reset in stop: it uses neither a queue nor a buffer.
	StelaVirtIOQueue *queues[] = {&rxQueue, &txQueue};
	for (StelaVirtIOQueue *queue : queues) {
		if (queue->memory) {
			queue->memory->complete();
			OSSafeReleaseNULL(queue->memory);
		}
	}
	release(&rx);
	release(&tx);
	OSSafeReleaseNULL(medium);
	// The family keeps and releases the output queue it asked for.
	outputQueue = nullptr;
	IOEthernetController::free();
}
