/*
 * One probe class per IONetworkingFamily class StelaVirtIO declares, each
 * overriding nothing: its vtable, which the constructor below makes clang
 * emit, holds the symbol the header puts in every slot, so xtask's family
 * test compares the header with networking.slots slot by slot. Built with
 * the kext's flags; never linked.
 */
#include "../IONetworking.h"

#define STELA_PROBE(Class)                                \
	class StelaProbe##Class : public Class {              \
	public:                                               \
		StelaProbe##Class();                              \
	};                                                    \
	StelaProbe##Class::StelaProbe##Class() : Class(nullptr) \
	{                                                     \
	}

STELA_PROBE(IONetworkController)
STELA_PROBE(IOEthernetController)
STELA_PROBE(IONetworkInterface)
STELA_PROBE(IOEthernetInterface)
STELA_PROBE(IOOutputQueue)
STELA_PROBE(IOBasicOutputQueue)
STELA_PROBE(IOGatedOutputQueue)
STELA_PROBE(IOPacketQueue)
STELA_PROBE(IONetworkMedium)
STELA_PROBE(IONetworkData)
