#include "Console.h"

#include <IOKit/IOLib.h>
#include <stela/virtio.h>

OSDefineMetaClassAndStructors(StelaVirtIOConsole, IOSerialDriverSync);

/* The virtio console's queues (virtio 1.2, 5.3.2): port 0's receiveq and transmitq. */
static const uint16_t kReceiveQueue = 0;
static const uint16_t kTransmitQueue = 1;
/* The slots' page, and the queues' entries: room for every slot. */
static const uint32_t kPage = 0x4000;
static const uint16_t kReceiveEntries = 8;
static const uint16_t kTransmitEntries = 16;
/* The node names IOSerialBSDClient makes: base, then suffix [mem]. */
static const char kBaseName[] = "stela-virtio";
static const char kSuffix[] = "-console";

static_assert(StelaVirtIOConsole::kTxSlots <= kTransmitEntries, "every transmit slot can be posted at once");
static_assert(StelaVirtIOConsole::kRxSlots <= kReceiveEntries, "every receive slot can be posted at once");
static_assert((StelaVirtIOConsole::kTxSlots + StelaVirtIOConsole::kRxSlots) * StelaVirtIOConsole::kSlotBytes <= kPage,
    "every slot is in the one page");
static_assert(StelaVirtIOConsole::kRxSlots * StelaVirtIOConsole::kSlotBytes <= StelaVirtIOConsole::kRxRingBytes,
    "the ring can take every receive slot at once");

/* The state a port starts with when acquired: both directions on, the
 * modem lines up so an open of the dial-in node finds its carrier. */
static const UInt32 kAcquiredState = PD_S_ACQUIRED | PD_S_TX_ENABLE | PD_S_RX_ENABLE | PD_RS232_S_CAR |
    PD_RS232_S_DSR | PD_RS232_S_CTS;

/* What a caller of the stream may set (setState): not ACQUIRED or ACTIVE,
 * which acquirePort and PD_E_ACTIVE own [mem: Apple's drivers refuse them]. */
static const UInt32 kExternalState = ~(PD_S_ACQUIRED | PD_S_ACTIVE);

struct StelaVirtIOConsole::Call {
	enum Kind {
		kAcquire,
		kRelease,
		kSetState,
		kGetState,
		kWatchState,
		kExecute,
		kRequest,
		kEnqueueData,
		kDequeueData,
	} kind;
	UInt32 a;
	UInt32 b;
	UInt32 *pointer;
	UInt8 *buffer;
	bool sleep;
	UInt32 value;
};

bool
StelaVirtIOConsole::start(IOService *provider)
{
	if (!IOSerialDriverSync::start(provider)) {
		return false;
	}
	nub = OSDynamicCast(StelaVirtIODevice, provider);
	if (!nub) {
		IOSerialDriverSync::stop(provider);
		return false;
	}
	features = nub->negotiate(STELA_VIRTIO_F_VERSION_1 | STELA_VIRTIO_F_EVENT_IDX);
	if (features == 0) {
		IOSerialDriverSync::stop(provider);
		return false;
	}
	buffers = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage);
	if (buffers && buffers->prepare() != kIOReturnSuccess) {
		OSSafeReleaseNULL(buffers);
	}
	IOByteCount bytes = 0;
	buffersPhysical = buffers ? buffers->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone) : 0;
	IOWorkLoop *workLoop = getWorkLoop();
	gate = workLoop ? IOCommandGate::commandGate(this) : nullptr;
	if (buffersPhysical == 0 || bytes < kPage || !gate || workLoop->addEventSource(gate) != kIOReturnSuccess) {
		IOLog("StelaVirtIOConsole: cannot allocate its slots or its gate\n");
		OSSafeReleaseNULL(gate);
		nub->fail();
		stop(provider);
		return false;
	}
	bzero(buffers->getBytesNoCopy(), kPage);
	if (!nub->createQueue(kReceiveQueue, kReceiveEntries, features, &receiveq) ||
	    !nub->createQueue(kTransmitQueue, kTransmitEntries, features, &transmitq)) {
		nub->fail();
		stop(provider);
		return false;
	}
	nub->setHandler(this, &StelaVirtIOConsole::interruptOccurred);
	nub->driverOK();
	// The receive slots go to the device at once, on the work loop as every
	// later post does: the host may have bytes already.
	workLoop->runAction(&StelaVirtIOConsole::begin, this);
	if (!publishStream()) {
		stop(provider);
		return false;
	}
	publishCounters();
	IOLog("StelaVirtIOConsole: %u transmit and %u receive slots of %u bytes, features %llx, /dev/cu.%s%s\n",
	    kTxSlots, kRxSlots, unsigned(kSlotBytes), features, kBaseName, kSuffix);
	registerService();
	return true;
}

/* The stream IOSerialBSDClient matches, attached to this driver. */
bool
StelaVirtIOConsole::publishStream()
{
	stream = OSTypeAlloc(IORS232SerialStreamSync);
	OSDictionary *noProperties = nullptr;
	// The refCon each call hands back: this driver has one port, so none.
	if (!stream || !stream->init(noProperties, nullptr) || !stream->attach(this)) {
		IOLog("StelaVirtIOConsole: cannot make its serial stream\n");
		OSSafeReleaseNULL(stream);
		return false;
	}
	// The family's own key symbols when it has made them, else their names.
	OSString *base = OSString::withCString(kBaseName);
	OSString *suffix = OSString::withCString(kSuffix);
	if (base && suffix) {
		if (gIOTTYBaseNameKey && gIOTTYSuffixKey) {
			stream->setProperty(gIOTTYBaseNameKey, base);
			stream->setProperty(gIOTTYSuffixKey, suffix);
		} else {
			stream->setProperty(kIOTTYBaseNameKey, base);
			stream->setProperty(kIOTTYSuffixKey, suffix);
		}
	}
	OSSafeReleaseNULL(base);
	OSSafeReleaseNULL(suffix);
	stream->registerService();
	return true;
}

uint8_t *
StelaVirtIOConsole::slot(unsigned index) const
{
	return static_cast<uint8_t *>(buffers->getBytesNoCopy()) + index * kSlotBytes;
}

uint64_t
StelaVirtIOConsole::slotPhysical(unsigned index) const
{
	return buffersPhysical + uint64_t(index) * kSlotBytes;
}

/* On the work loop: every receive slot to the device. */
IOReturn
StelaVirtIOConsole::begin(OSObject *target, void *, void *, void *, void *)
{
	auto console = static_cast<StelaVirtIOConsole *>(target);
	console->running = true;
	stela_vq_arm(&console->receiveq.vq);
	stela_vq_arm(&console->transmitq.vq);
	console->postReceive();
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOConsole::halt(OSObject *target, void *, void *, void *, void *)
{
	auto console = static_cast<StelaVirtIOConsole *>(target);
	console->running = false;
	// Every waiter wakes to a port that is gone.
	console->portState = 0;
	if (console->gate) {
		console->gate->commandWakeup(&console->portState, false);
	}
	return kIOReturnSuccess;
}

void
StelaVirtIOConsole::stop(IOService *provider)
{
	if (stream) {
		stream->terminate();
		OSSafeReleaseNULL(stream);
	}
	IOWorkLoop *workLoop = getWorkLoop();
	if (workLoop) {
		workLoop->runAction(&StelaVirtIOConsole::halt, this);
	}
	if (nub) {
		nub->setHandler(nullptr, nullptr);
		// Once the reset reads back the device uses no slot.
		nub->reset();
	}
	if (gate) {
		workLoop->removeEventSource(gate);
		OSSafeReleaseNULL(gate);
	}
	IOSerialDriverSync::stop(provider);
}

void
StelaVirtIOConsole::free()
{
	// The device was reset in stop: it uses neither the queues nor a slot.
	StelaVirtIOQueue *queues[] = {&receiveq, &transmitq};
	for (StelaVirtIOQueue *queue : queues) {
		if (queue->memory) {
			queue->memory->complete();
			OSSafeReleaseNULL(queue->memory);
		}
	}
	if (buffers) {
		buffers->complete();
		OSSafeReleaseNULL(buffers);
	}
	IOSerialDriverSync::free();
}

/* The device's interrupt, on the work loop with the gate held. */
void
StelaVirtIOConsole::interruptOccurred(OSObject *owner, uint32_t status)
{
	auto console = static_cast<StelaVirtIOConsole *>(owner);
	if (!console->running) {
		return;
	}
	if (status & STELA_VIRTIO_MMIO_INT_VRING) {
		console->harvestTransmit();
		console->harvestReceive();
		console->updateQueues();
		console->publishCounters();
	}
}

void
StelaVirtIOConsole::harvestTransmit()
{
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&transmitq.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			stela_vq_arm(&transmitq.vq);
			if (!stela_vq_pending(&transmitq.vq)) {
				return;
			}
			continue;
		}
		unsigned index = unsigned(static_cast<uint8_t *>(token) - txTokens);
		if (found == STELA_VQ_BROKEN || index >= kTxSlots) {
			IOLog("StelaVirtIOConsole: the device used a transmit buffer it was not given\n");
			running = false;
			nub->fail();
			return;
		}
		txBusy[index] = false;
	}
}

void
StelaVirtIOConsole::harvestReceive()
{
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&receiveq.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			stela_vq_arm(&receiveq.vq);
			if (!stela_vq_pending(&receiveq.vq)) {
				break;
			}
			continue;
		}
		unsigned index = unsigned(static_cast<uint8_t *>(token) - rxTokens);
		if (found == STELA_VQ_BROKEN || index >= kRxSlots) {
			IOLog("StelaVirtIOConsole: the device used a receive buffer it was not given\n");
			running = false;
			nub->fail();
			return;
		}
		rxPosted[index] = false;
		if (written > kSlotBytes) {
			written = kSlotBytes;
		}
		// The ring had room for every posted slot (postReceive), so this fits.
		const uint8_t *bytes = slot(kTxSlots + index);
		for (uint32_t at = 0; at < written; at++) {
			if (receivedCount == kRxRingBytes) {
				receiveDropped += written - at;
				break;
			}
			received[(receivedHead + receivedCount) % kRxRingBytes] = bytes[at];
			receivedCount++;
		}
		bytesReceived += written;
	}
	postReceive();
}

/* Posts free receive slots while the ring could take every posted one full. */
void
StelaVirtIOConsole::postReceive()
{
	if (!running) {
		return;
	}
	unsigned posted = 0;
	for (unsigned index = 0; index < kRxSlots; index++) {
		posted += rxPosted[index];
	}
	bool added = false;
	for (unsigned index = 0; index < kRxSlots; index++) {
		if (rxPosted[index]) {
			continue;
		}
		if (receivedCount + (posted + 1) * kSlotBytes > kRxRingBytes) {
			break;
		}
		struct stela_vq_buf buffer = {slotPhysical(kTxSlots + index), kSlotBytes};
		if (stela_vq_add(&receiveq.vq, &buffer, 0, 1, &rxTokens[index]) < 0) {
			break;
		}
		rxPosted[index] = true;
		posted++;
		added = true;
	}
	if (added) {
		nub->kick(&receiveq);
	}
}

void
StelaVirtIOConsole::updateQueues()
{
	unsigned busy = 0;
	for (unsigned index = 0; index < kTxSlots; index++) {
		busy += txBusy[index];
	}
	UInt32 queues = 0;
	if (busy == 0) {
		queues |= PD_S_TXQ_EMPTY;
	} else {
		queues |= PD_S_TX_BUSY;
	}
	if (busy <= kTxSlots / 4) {
		queues |= PD_S_TXQ_LOW_WATER;
	}
	if (busy >= kTxSlots * 3 / 4) {
		queues |= PD_S_TXQ_HIGH_WATER;
	}
	if (busy == kTxSlots) {
		queues |= PD_S_TXQ_FULL;
	}
	if (receivedCount == 0) {
		queues |= PD_S_RXQ_EMPTY;
	}
	if (receivedCount <= kRxRingBytes / 4) {
		queues |= PD_S_RXQ_LOW_WATER;
	}
	if (receivedCount >= kRxRingBytes * 3 / 4) {
		queues |= PD_S_RXQ_HIGH_WATER;
	}
	if (receivedCount == kRxRingBytes) {
		queues |= PD_S_RXQ_FULL;
	}
	const UInt32 mask = PD_S_TXQ_MASK | PD_S_RXQ_MASK | PD_S_TX_BUSY;
	// An unacquired port has no queues to show.
	if (!(portState & PD_S_ACQUIRED)) {
		queues = 0;
	}
	setStateGated(queues, mask);
}

void
StelaVirtIOConsole::setStateGated(UInt32 state, UInt32 mask)
{
	UInt32 now = (portState & ~mask) | (state & mask);
	if (now != portState) {
		portState = now;
		gate->commandWakeup(&portState, false);
	}
}

/*
 * Waits until any bit of `mask` in the state equals that bit of `*state`,
 * and answers the state then. Unless the caller watches ACQUIRED or ACTIVE,
 * the port going inactive ends the wait with kIOReturnIOError, as
 * IOSerialBSDClient expects of its threads [mem: Apple's watchState; the
 * client's code tests kIOReturnIOError after dequeueData, kc].
 */
IOReturn
StelaVirtIOConsole::watchStateGated(UInt32 *state, UInt32 mask)
{
	UInt32 wanted = *state;
	bool active = false;
	if (!(mask & (PD_S_ACQUIRED | PD_S_ACTIVE))) {
		wanted &= ~PD_S_ACTIVE;
		mask |= PD_S_ACTIVE;
		active = true;
	}
	for (;;) {
		UInt32 found = (wanted ^ ~portState) & mask;
		if (found) {
			*state = portState;
			return (active && (found & PD_S_ACTIVE)) ? kIOReturnIOError : kIOReturnSuccess;
		}
		IOReturn woken = gate->commandSleep(&portState);
		if (woken == THREAD_INTERRUPTED || woken == THREAD_RESTART) {
			return kIOReturnAborted;
		}
		if (woken != THREAD_AWAKENED) {
			return kIOReturnIOError;
		}
	}
}

IOReturn
StelaVirtIOConsole::acquireGated(bool sleep)
{
	for (;;) {
		if (!running) {
			return kIOReturnOffline;
		}
		if (!(portState & PD_S_ACQUIRED)) {
			portState = kAcquiredState;
			eventsKept = 0;
			updateQueues();
			gate->commandWakeup(&portState, false);
			return kIOReturnSuccess;
		}
		if (!sleep) {
			return kIOReturnExclusiveAccess;
		}
		UInt32 free = 0;
		IOReturn watched = watchStateGated(&free, PD_S_ACQUIRED);
		if (watched != kIOReturnSuccess) {
			return watched;
		}
	}
}

IOReturn
StelaVirtIOConsole::releaseGated()
{
	if (!(portState & PD_S_ACQUIRED)) {
		return kIOReturnNotOpen;
	}
	// Bytes already handed to the device go out; the port forgets the rest.
	portState = 0;
	gate->commandWakeup(&portState, false);
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOConsole::executeGated(UInt32 event, UInt32 data)
{
	switch (event) {
	case PD_E_ACTIVE:
		if (!(portState & PD_S_ACQUIRED)) {
			return kIOReturnNotOpen;
		}
		setStateGated(data ? PD_S_ACTIVE : 0, PD_S_ACTIVE);
		return kIOReturnSuccess;
	case PD_E_RXQ_FLUSH:
		flushReceived();
		return kIOReturnSuccess;
	case PD_E_TXQ_FLUSH:
		// What enqueueData took is the device's already: nothing waits here.
		return kIOReturnSuccess;
	default:
		break;
	}
	// Line settings virtio-console has none of: kept for requestEvent only.
	for (unsigned index = 0; index < eventsKept; index++) {
		if (eventCodes[index] == event) {
			eventData[index] = data;
			return kIOReturnSuccess;
		}
	}
	if (eventsKept < kEventsKept) {
		eventCodes[eventsKept] = event;
		eventData[eventsKept] = data;
		eventsKept++;
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOConsole::requestGated(UInt32 event, UInt32 *data)
{
	if (!data) {
		return kIOReturnBadArgument;
	}
	uint32_t free = 0;
	for (unsigned index = 0; index < kTxSlots; index++) {
		free += !txBusy[index];
	}
	switch (event) {
	case PD_E_TXQ_SIZE:
		*data = kTxSlots * kSlotBytes;
		return kIOReturnSuccess;
	case PD_E_RXQ_SIZE:
		*data = kRxRingBytes;
		return kIOReturnSuccess;
	case PD_E_TXQ_LOW_WATER:
		*data = kTxSlots * kSlotBytes / 4;
		return kIOReturnSuccess;
	case PD_E_RXQ_LOW_WATER:
		*data = kRxRingBytes / 4;
		return kIOReturnSuccess;
	case PD_E_TXQ_HIGH_WATER:
		*data = kTxSlots * kSlotBytes * 3 / 4;
		return kIOReturnSuccess;
	case PD_E_RXQ_HIGH_WATER:
		*data = kRxRingBytes * 3 / 4;
		return kIOReturnSuccess;
	case PD_E_TXQ_AVAILABLE:
		// Whole free slots: enqueueData takes this much without sleeping.
		*data = free * kSlotBytes;
		return kIOReturnSuccess;
	case PD_E_RXQ_AVAILABLE:
		*data = receivedCount;
		return kIOReturnSuccess;
	default:
		break;
	}
	*data = 0;
	for (unsigned index = 0; index < eventsKept; index++) {
		if (eventCodes[index] == event) {
			*data = eventData[index];
		}
	}
	// No flow control unless set; any other setting reads back as set.
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOConsole::enqueueGated(UInt8 *buffer, UInt32 size, UInt32 *count, bool sleep)
{
	*count = 0;
	if (!running) {
		return kIOReturnOffline;
	}
	if (!(portState & PD_S_ACTIVE)) {
		return kIOReturnNotOpen;
	}
	bool added = false;
	while (*count < size && running) {
		unsigned index = 0;
		while (index < kTxSlots && txBusy[index]) {
			index++;
		}
		if (index == kTxSlots) {
			if (added) {
				nub->kick(&transmitq);
				added = false;
				updateQueues();
			}
			if (!sleep) {
				break;
			}
			UInt32 notFull = 0;
			IOReturn watched = watchStateGated(&notFull, PD_S_TXQ_FULL);
			if (watched != kIOReturnSuccess) {
				return watched;
			}
			continue;
		}
		UInt32 bytes = size - *count;
		if (bytes > kSlotBytes) {
			bytes = kSlotBytes;
		}
		bcopy(buffer + *count, slot(index), bytes);
		struct stela_vq_buf chain = {slotPhysical(index), bytes};
		if (stela_vq_add(&transmitq.vq, &chain, 1, 0, &txTokens[index]) < 0) {
			break;
		}
		txBusy[index] = true;
		added = true;
		*count += bytes;
		bytesTransmitted += bytes;
	}
	if (added) {
		nub->kick(&transmitq);
	}
	updateQueues();
	return kIOReturnSuccess;
}

UInt32
StelaVirtIOConsole::take(UInt8 *buffer, UInt32 size)
{
	UInt32 taken = 0;
	while (taken < size && receivedCount > 0) {
		buffer[taken++] = received[receivedHead];
		receivedHead = (receivedHead + 1) % kRxRingBytes;
		receivedCount--;
	}
	if (taken > 0) {
		postReceive();
		updateQueues();
	}
	return taken;
}

void
StelaVirtIOConsole::flushReceived()
{
	receivedHead = 0;
	receivedCount = 0;
	postReceive();
	updateQueues();
}

/*
 * Takes what is received, then, while fewer than `min` bytes came, waits
 * for more: the reader's thread sleeps until the ring has bytes or the port
 * goes inactive [mem: Apple's dequeueData].
 */
IOReturn
StelaVirtIOConsole::dequeueGated(UInt8 *buffer, UInt32 size, UInt32 *count, UInt32 min)
{
	*count = 0;
	if (!(portState & PD_S_ACTIVE)) {
		return kIOReturnNotOpen;
	}
	*count = take(buffer, size);
	while (min > 0 && *count < min && *count < size) {
		UInt32 notEmpty = 0;
		IOReturn watched = watchStateGated(&notEmpty, PD_S_RXQ_EMPTY);
		if (watched != kIOReturnSuccess) {
			return watched;
		}
		*count += take(buffer + *count, size - *count);
	}
	return kIOReturnSuccess;
}

void
StelaVirtIOConsole::publishCounters()
{
	setProperty("BytesTransmitted", bytesTransmitted, 64);
	setProperty("BytesReceived", bytesReceived, 64);
	setProperty("ReceiveDropped", receiveDropped, 64);
}

IOReturn
StelaVirtIOConsole::gated(OSObject *owner, void *argument, void *, void *, void *)
{
	auto console = static_cast<StelaVirtIOConsole *>(owner);
	auto call = static_cast<Call *>(argument);
	switch (call->kind) {
	case Call::kAcquire:
		return console->acquireGated(call->sleep);
	case Call::kRelease:
		return console->releaseGated();
	case Call::kSetState:
		if (call->b & ~kExternalState) {
			return kIOReturnBadArgument;
		}
		console->setStateGated(call->a, call->b);
		return kIOReturnSuccess;
	case Call::kGetState:
		console->updateQueues();
		call->value = console->portState;
		return kIOReturnSuccess;
	case Call::kWatchState:
		return console->watchStateGated(call->pointer, call->b);
	case Call::kExecute:
		return console->executeGated(call->a, call->b);
	case Call::kRequest:
		return console->requestGated(call->a, call->pointer);
	case Call::kEnqueueData:
		return console->enqueueGated(call->buffer, call->a, call->pointer, call->sleep);
	case Call::kDequeueData:
		return console->dequeueGated(call->buffer, call->a, call->pointer, call->b);
	}
	return kIOReturnBadArgument;
}

IOReturn
StelaVirtIOConsole::run(Call *call)
{
	if (!gate) {
		return kIOReturnOffline;
	}
	return gate->runAction(&StelaVirtIOConsole::gated, call);
}

IOReturn
StelaVirtIOConsole::acquirePort(bool sleep, void *)
{
	Call call = {Call::kAcquire, 0, 0, nullptr, nullptr, sleep, 0};
	return run(&call);
}

IOReturn
StelaVirtIOConsole::releasePort(void *)
{
	Call call = {Call::kRelease, 0, 0, nullptr, nullptr, false, 0};
	return run(&call);
}

IOReturn
StelaVirtIOConsole::setState(UInt32 state, UInt32 mask, void *)
{
	Call call = {Call::kSetState, state, mask, nullptr, nullptr, false, 0};
	return run(&call);
}

UInt32
StelaVirtIOConsole::getState(void *)
{
	Call call = {Call::kGetState, 0, 0, nullptr, nullptr, false, 0};
	return run(&call) == kIOReturnSuccess ? call.value : 0;
}

IOReturn
StelaVirtIOConsole::watchState(UInt32 *state, UInt32 mask, void *)
{
	if (!state) {
		return kIOReturnBadArgument;
	}
	Call call = {Call::kWatchState, 0, mask, state, nullptr, false, 0};
	return run(&call);
}

UInt32
StelaVirtIOConsole::nextEvent(void *)
{
	// No events are queued: line settings take effect at once.
	return PD_E_EOQ;
}

IOReturn
StelaVirtIOConsole::executeEvent(UInt32 event, UInt32 data, void *)
{
	Call call = {Call::kExecute, event, data, nullptr, nullptr, false, 0};
	return run(&call);
}

IOReturn
StelaVirtIOConsole::requestEvent(UInt32 event, UInt32 *data, void *)
{
	Call call = {Call::kRequest, event, 0, data, nullptr, false, 0};
	return run(&call);
}

/* An event queued is an event executed [mem: as Apple's drivers do]. */
IOReturn
StelaVirtIOConsole::enqueueEvent(UInt32 event, UInt32 data, bool, void *refCon)
{
	return executeEvent(event, data, refCon);
}

IOReturn
StelaVirtIOConsole::dequeueEvent(UInt32 *event, UInt32 *data, bool, void *refCon)
{
	if (!event || !data) {
		return kIOReturnBadArgument;
	}
	*event = PD_E_EOQ;
	*data = 0;
	return getState(refCon) & PD_S_ACTIVE ? kIOReturnSuccess : kIOReturnNotOpen;
}

IOReturn
StelaVirtIOConsole::enqueueData(UInt8 *buffer, UInt32 size, UInt32 *count, bool sleep, void *)
{
	if (!buffer || !count) {
		return kIOReturnBadArgument;
	}
	Call call = {Call::kEnqueueData, size, 0, count, buffer, sleep, 0};
	return run(&call);
}

IOReturn
StelaVirtIOConsole::dequeueData(UInt8 *buffer, UInt32 size, UInt32 *count, UInt32 min, void *)
{
	if (!buffer || !count) {
		return kIOReturnBadArgument;
	}
	Call call = {Call::kDequeueData, size, min, count, buffer, false, 0};
	return run(&call);
}
