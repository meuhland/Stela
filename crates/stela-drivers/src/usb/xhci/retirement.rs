// SPDX-License-Identifier: GPL-2.0-only
//! Whole-slot retirement before Disable Slot (xHCI 4.6.4).
//!
//! Linux xhci_free_dev receives a device whose traffic was already stopped by
//! usb_disconnect. Native enumeration failures must establish that same fact;
//! an unpublished descriptor inventory does not mean EP0 stopped running.
use super::*;
use reset::Ownership;

type Failure = (&'static str, BlockError);
const ENDPOINT_DISABLED: u32 = 0;

/// Minted only after the whole-slot retirement boundary. Its fields are
/// private to this module, so a software status or slot number cannot return DMA.
pub(super) struct Unpublished {
    slot: u8,
    state: Slot,
}

impl Unpublished {
    pub(super) fn into_slot(self) -> (u8, Slot) {
        (self.slot, self.state)
    }
}

/// Retired leaf owner, never reconstructed from a current ledger: on a root
/// port, or behind hubs with the parent port it is reset through. The
/// original Slot stays in Controller until physical reset and final health.
struct RetiredLeaf {
    id: SlotId,
    attachment: Attachment,
    downstream: Option<reset::DownstreamPort>,
}

/// Complete published storage returned only by RetiredLeaf::reset.
pub(super) struct Published {
    slot: u8,
    state: Slot,
}
impl Published {
    pub(super) fn into_slot(self) -> (u8, Slot) {
        (self.slot, self.state)
    }
}

/// The mutable controller borrow is held from the last acknowledged
/// endpoint retirement through Disable Slot. No caller can publish another TD,
/// run private recovery control, or substitute a different device generation
/// between those operations. This permission is private and cannot be copied.
struct Quiesced<'a> {
    controller: &'a mut Controller,
    slot: u8,
    ownership: Ownership,
}

impl Controller {
    /// Exclusive leaf reset is the only published storage return producer.
    /// Its private witness keeps the original generation/current/attachment
    /// through whole-slot retirement and postsettle physical reset: a root
    /// port's, or `downstream`, the parent hub port of a device behind hubs.
    #[expect(
        clippy::result_large_err,
        reason = "The shared error preserves exact source and cleanup causes without allocating on propagation."
    )]
    pub(super) fn retire_reset_leaf(
        &mut self,
        id: SlotId,
        downstream: Option<reset::DownstreamPort>,
    ) -> Result<Published, Failure> {
        let slot = self
            .validate_id(id)
            .map_err(|e| ("validate reusable generation", e))?;
        let state = self.slots[usize::from(slot)].as_ref().unwrap();
        let attachment = state.attachment;
        let path = match (&downstream, attachment.parent) {
            (None, None) => {
                state.parent_id.is_none()
                    && attachment.tier == 0
                    && attachment.route == 0
                    && attachment.translator.is_none()
            }
            (Some(downstream), Some((_, port))) => {
                state.parent_id == Some(downstream.parent)
                    && downstream.port == port
                    && attachment.tier != 0
                    && attachment.route != 0
            }
            _ => false,
        };
        if !path || !self.retry_resources.can_offer(slot, state) {
            return Err((
                "preflight reusable leaf storage",
                BlockError::Unsupported(
                    "USB reusable reset requires one complete leaf resource pool",
                ),
            ));
        }
        self.disable_slot(slot, Ownership::Retired)?;
        RetiredLeaf {
            id,
            attachment,
            downstream,
        }
        .reset(self)
    }

    /// Retire every endpoint and its ordered events, then acknowledge
    /// Disable before clearing DCBAA. Generic Disable keeps published owners
    /// permanently; only unpublished EP0-only owners enter the retry cache.
    /// The separate RetiredLeaf witness additionally requires physical reset
    /// and final health before Published can return complete leaf storage.
    #[expect(
        clippy::result_large_err,
        reason = "The shared error preserves exact source and cleanup causes without allocating on propagation."
    )]
    pub(super) fn disable_slot(&mut self, slot: u8, ownership: Ownership) -> Result<(), Failure> {
        Quiesced::acquire(self, slot, ownership)?.disable()
    }
}

impl RetiredLeaf {
    #[expect(
        clippy::result_large_err,
        reason = "The shared error preserves exact source and cleanup causes without allocating on propagation."
    )]
    fn reset(self, controller: &mut Controller) -> Result<Published, Failure> {
        let slot = self.id.hardware_slot();
        let index = usize::from(slot);
        let root = self.attachment.root_port;
        let port = self
            .downstream
            .as_ref()
            .map(|downstream| (downstream.parent, downstream.port));
        // The typed witness binds the Disable to these original owners;
        // the current grant is still present before the physical reset.
        if !controller.known_id(self.id)
            || controller.slot_ownership[index] != Ownership::Retired
            || controller.dcbaa.read::<u64>(index * 8) != 0
            || controller
                .current_port_owner(port, root)
                .map_err(|e| ("retain reusable physical owner", e))?
                != Some(self.id)
        {
            return Err((
                "retain reusable physical owner",
                BlockError::Unsupported("USB reusable reset lost the original retired attachment"),
            ));
        }
        match &self.downstream {
            None => controller
                .reset_root_device(self.attachment)
                .map_err(|e| ("acknowledge reusable root reset", e))?,
            // The hub port's own status is checked there: enabled, no
            // change, the original speed.
            Some(downstream) => controller.reset_downstream_port(downstream, self.attachment)?,
        }
        // Both resets include their settling delay. Recheck health and link
        // after that delay, before erasing a single retired DMA byte.
        controller
            .ready()
            .map_err(|e| ("post-reset reusable controller health", e))?;
        let link = match &self.downstream {
            None => {
                let status = controller.registers.read32(controller.port_offset(root));
                status & (CCS | PED | PP) == CCS | PED | PP
                    && status & (PR | PORT_CHANGES | (1 << 3)) == 0
                    && (status >> PORT_SPEED_SHIFT & PORT_SPEED_FIELD) as u8
                        == self.attachment.speed
                    && status & PLS == 0
            }
            Some(_) => true,
        };
        let raw = port.map(|(parent, port)| (parent.hardware_slot(), port));
        if !link
            || controller.power.port_owner(raw, root).is_some()
            || controller
                .pending
                .iter()
                .any(|e| e.kind() == TRANSFER_EVENT && e.slot() == slot)
        {
            return Err((
                "verify reusable reset settlement",
                BlockError::Unsupported("USB reusable reset lacks a stable original physical link"),
            ));
        }
        let state = controller.slots[index].take().unwrap();
        controller.slot_ownership[index] = Ownership::Vacant;
        Ok(Published { slot, state })
    }
}

impl<'a> Quiesced<'a> {
    #[expect(
        clippy::result_large_err,
        reason = "The shared error preserves exact source and cleanup causes without allocating on propagation."
    )]
    fn acquire(
        controller: &'a mut Controller,
        slot: u8,
        ownership: Ownership,
    ) -> Result<Self, Failure> {
        let index = usize::from(slot);
        if slot == 0
            || index >= controller.slots.len()
            || controller.slot_ownership.get(index) != Some(&Ownership::Enabled)
            || !matches!(ownership, Ownership::Vacant | Ownership::Retired)
        {
            return Err((
                "preflight Disable Slot ownership",
                BlockError::Unsupported("invalid xHCI slot retirement owner"),
            ));
        }
        let state = controller.slots.get_mut(index).and_then(Option::as_mut);
        let unaddressed = if let Some(state) = state {
            if ownership == Ownership::Vacant
                && (state.inventory.is_some()
                    || state.consumer != identity::Consumer::Unclaimed
                    || state
                        .endpoints
                        .iter()
                        .enumerate()
                        .any(|(dci, endpoint)| dci != 1 && endpoint.is_some())
                    || state.retired_rings.iter().any(Option::is_some)
                    || state.endpoints[1]
                        .as_ref()
                        .is_none_or(|ep0| ep0.streams.is_some() || ep0.iso.is_some()))
            {
                return Err((
                    "preflight unpublished Disable Slot",
                    BlockError::Unsupported("published xHCI slot cannot become vacant"),
                ));
            }
            // Address Device has not succeeded and no EP0 doorbell could have
            // been published while this initial generation fence was held.
            // Such a failed command may leave the zeroed EP0 context Disabled;
            // Stop/Set Dequeue would be illegal in that hardware state.
            let unaddressed = ownership == Ownership::Vacant
                && state.retiring
                && state.inventory.is_none()
                && state.consumer == identity::Consumer::Unclaimed
                && state.current_configuration == Some(0)
                && state.endpoints.iter().skip(2).all(Option::is_none)
                && state.endpoints[1].as_ref().is_some_and(|endpoint| {
                    endpoint.ring.index == 0
                        && endpoint.ring.cycle
                        && endpoint.in_flight == 0
                        && endpoint.streams.is_none()
                        && endpoint.iso.is_none()
                });
            state.retiring = true;
            unaddressed
        } else {
            // The Enable Slot grant may need returning when reservation failed
            // before any contexts were published. Never infer this from absence
            // of a software record if DCBAA still names a DMA allocation.
            if ownership != Ownership::Vacant || controller.dcbaa.read::<u64>(index * 8) != 0 {
                return Err((
                    "preflight unaddressed Disable Slot",
                    BlockError::Unsupported("xHCI slot allocation ownership unresolved"),
                ));
            }
            true
        };
        controller
            .ready()
            .map_err(|error| ("pre-Disable Slot controller status", error))?;
        let context = controller.context();
        let mut failure = None;
        for dci in 1..32usize {
            let Some(state) = controller.slots[index].as_ref() else {
                break;
            };
            if state.endpoints[dci].is_none() {
                continue;
            }
            fence(Ordering::Acquire);
            let endpoint_state = state
                ._output
                .coherent_u32(dci * context)
                .map_err(|error| ("observe actual endpoint DMA context", error))?
                & 7;
            if unaddressed && dci == 1 && endpoint_state == ENDPOINT_DISABLED {
                continue;
            }
            // The complete software endpoint set is visited even after one
            // contributor fails. No Disable Slot is published on partial success.
            if let Err(error) = controller.cancel_inner(slot, dci as u8) {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(("retire every endpoint before Disable Slot", error));
        }
        controller
            .ready()
            .map_err(|error| ("pre-Disable Slot event retirement", error))?;
        // Recheck device contexts after every cancellation and its event
        // drain, rather than accepting a software queue count as an idle proof.
        if let Some(state) = controller.slots[index].as_ref() {
            fence(Ordering::Acquire);
            for (dci, endpoint) in state.endpoints.iter().enumerate().skip(1) {
                if let Some(endpoint) = endpoint {
                    let actual = state
                        ._output
                        .coherent_u32(dci * context)
                        .map_err(|error| ("observe actual endpoint DMA context", error))?
                        & 7;
                    if endpoint.poisoned
                        || ownership == Ownership::Vacant
                            && (endpoint.in_flight != 0 || !endpoint.ring.normal.idle())
                        || !(actual == ENDPOINT_STOPPED
                            || unaddressed && dci == 1 && actual == ENDPOINT_DISABLED)
                    {
                        return Err((
                            "verify stopped endpoint before Disable Slot",
                            BlockError::Unsupported("xHCI slot endpoint retirement unresolved"),
                        ));
                    }
                }
            }
        }
        Ok(Self {
            controller,
            slot,
            ownership,
        })
    }

    #[expect(
        clippy::result_large_err,
        reason = "The shared error preserves exact source and cleanup causes without allocating on propagation."
    )]
    fn disable(self) -> Result<(), Failure> {
        let Self {
            controller,
            slot,
            ownership,
        } = self;
        let completion = controller
            .command(0, 0, reset::DISABLE_SLOT << 10 | u32::from(slot) << 24)
            .map_err(|error| ("acknowledge Disable Slot", error))?;
        if completion.slot() != slot {
            controller.command_poisoned = true;
            return Err((
                "acknowledge Disable Slot identity",
                BlockError::Unsupported("Disable Slot completion named another slot"),
            ));
        }
        #[cfg(test)]
        {
            controller.test_retired_numbers[usize::from(slot)] = true;
        }
        controller
            .ready()
            .map_err(|error| ("post-Disable Slot controller status", error))?;
        controller
            .pending
            .retain(|event| !(event.kind() == TRANSFER_EVENT && event.slot() == slot));
        controller.slot_ownership[usize::from(slot)] = ownership;
        controller.dcbaa.write(usize::from(slot) * 8, 0u64);
        if ownership == Ownership::Vacant
            && let Some(state) = controller.slots[usize::from(slot)].take()
        {
            controller
                .retry_resources
                .retain(Unpublished { slot, state });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
