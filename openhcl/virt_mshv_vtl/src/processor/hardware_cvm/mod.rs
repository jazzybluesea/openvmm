// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common processor support for hardware-isolated partitions.

mod tlb_lock;

use super::UhProcessor;
use crate::processor::UhHypercallHandler;
use crate::validate_vtl_gpa_flags;
use crate::GuestVsmState;
use crate::GuestVsmVtl1State;
use crate::GuestVsmVtl1StateInner;
use crate::GuestVtl;
use crate::HardwareIsolatedBacking;
use crate::WakeReason;
use hvdef::hypercall::HvFlushFlags;
use hvdef::HvError;
use hvdef::HvMapGpaFlags;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvResult;
use hvdef::Vtl;
use std::iter::zip;
use virt::io::CpuIo;
use zerocopy::FromZeroes;

impl<T: CpuIo, B: HardwareIsolatedBacking> UhHypercallHandler<'_, '_, T, B> {
    pub fn hcvm_enable_partition_vtl(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        flags: hvdef::hypercall::EnablePartitionVtlFlags,
    ) -> HvResult<()> {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        let target_vtl = GuestVtl::try_from(target_vtl).map_err(|_| HvError::AccessDenied)?;
        if target_vtl != GuestVtl::Vtl1 {
            return Err(HvError::AccessDenied);
        }

        if flags.enable_supervisor_shadow_stack() || flags.enable_hardware_hvpt() {
            return Err(HvError::InvalidParameter);
        }

        let mut gvsm_state = self.vp.partition.guest_vsm.write();

        match *gvsm_state {
            GuestVsmState::NotPlatformSupported => return Err(HvError::AccessDenied),
            GuestVsmState::NotGuestEnabled => (),
            GuestVsmState::Enabled { vtl1: _ } => {
                // VTL 1 cannot be already enabled
                return Err(HvError::VtlAlreadyEnabled);
            }
        }

        self.vp.partition.hcl.enable_partition_vtl(
            target_vtl,
            // These flags are managed and enforced internally; CVMs can't rely
            // on the hypervisor
            0.into(),
        )?;

        *gvsm_state = GuestVsmState::Enabled {
            vtl1: GuestVsmVtl1State {
                enable_vtl_protection: false,
                inner: GuestVsmVtl1StateInner::HardwareCvm {
                    state: crate::HardwareCvmVtl1State {
                        mbec_enabled: flags.enable_mbec(),
                        ..Default::default()
                    },
                },
            },
        };

        let protector = self
            .vp
            .partition
            .isolated_memory_protector
            .as_ref()
            .expect("exists for a cvm");

        // Grant VTL 1 access to lower VTL memory
        tracing::debug!("Granting VTL 1 access to lower VTL memory");
        protector
            .change_default_vtl_protections(GuestVtl::Vtl1, hvdef::HV_MAP_GPA_PERMISSIONS_ALL)?;

        tracing::debug!("Successfully granted vtl 1 access to lower vtl memory");

        tracing::info!("Enabled vtl 1 on the partition");

        Ok(())
    }

    pub fn hcvm_enable_vp_vtl(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> HvResult<()> {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        if vp_index as usize >= self.vp.partition.vps.len() {
            return Err(HvError::InvalidVpIndex);
        }

        let vtl = GuestVtl::try_from(vtl).map_err(|_| HvError::InvalidParameter)?;
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        // If handling on behalf of VTL 0, then lock to make sure that no other
        // VP makes this call on behalf of VTL 0.
        let gvsm_state = {
            let mut gvsm_state = self.vp.partition.guest_vsm.write();

            // Should be enabled on the partition
            let vtl1_state = gvsm_state.get_vtl1_mut().ok_or(HvError::InvalidVtlState)?;
            let vtl1_state_inner = vtl1_state.inner.get_hardware_cvm_mut().unwrap();

            let current_vp_index = self.vp.vp_index().index();

            // A higher VTL can only be enabled on the current processor to make
            // sure that the lower VTL is executing at a known point, and only if
            // the higher VTL has not been enabled on any other VP because at that
            // point, the higher VTL should be orchestrating its own enablement.
            //
            // TODO GUEST_VSM: last_vtl currently always returns 0 (which is wrong),
            // so for any VP outside of the BSP, this will fail
            if self.intercepted_vtl < GuestVtl::Vtl1 {
                if vtl1_state_inner.enabled_on_vp_count > 0 || vp_index != current_vp_index {
                    return Err(HvError::AccessDenied);
                }

                Some(gvsm_state)
            } else {
                // If handling on behalf of VTL 1, then some other VP (i.e. the
                // bsp) must have already handled EnableVpVtl. No partition-wide
                // state is changing, so no need to hold the lock
                assert!(vtl1_state_inner.enabled_on_vp_count > 0);
                None
            }
        };

        // Lock the remote vp state to make sure no other VP is trying to enable
        // VTL 1 on it.
        let target_vp = &self.vp.partition.vps[vp_index as usize];
        let mut vtl1_enabled = target_vp.hcvm_vtl1_enabled.lock();

        if *vtl1_enabled {
            return Err(HvError::VtlAlreadyEnabled);
        }

        // TODO GUEST_VSM: construct APIC (including overlays, vp assist page) for VTL 1

        // Register the VMSA with the hypervisor
        let hv_vp_context = match self.vp.partition.isolation {
            virt::IsolationType::None | virt::IsolationType::Vbs => unreachable!(),
            virt::IsolationType::Snp => {
                // For VTL 1, user mode needs to explicitly register the VMSA
                // with the hypervisor via the EnableVpVtl hypercall.
                let vmsa_pfn = self.vp.partition.hcl.vtl1_vmsa_pfn(vp_index);
                let sev_control = hvdef::HvX64RegisterSevControl::new()
                    .with_enable_encrypted_state(true)
                    .with_vmsa_gpa_page_number(vmsa_pfn);

                let mut hv_vp_context = hvdef::hypercall::InitialVpContextX64::new_zeroed();
                hv_vp_context.rip = sev_control.into();

                hv_vp_context
            }
            virt::IsolationType::Tdx => {
                // TODO GUEST VSM
                hvdef::hypercall::InitialVpContextX64::new_zeroed()
            }
        };

        self.vp
            .partition
            .hcl
            .enable_vp_vtl(vp_index, vtl, hv_vp_context)?;

        // Cannot fail from here
        if let Some(mut gvsm) = gvsm_state {
            gvsm.get_vtl1_mut()
                .unwrap()
                .inner
                .get_hardware_cvm_mut()
                .unwrap()
                .enabled_on_vp_count += 1;
        }

        *vtl1_enabled = true;

        let target_vp = &self.vp.partition.vps[vp_index as usize];
        *target_vp.hv_start_enable_vtl_vp[vtl].lock() = Some(Box::new(*vp_context));
        target_vp.wake(vtl, WakeReason::HV_START_ENABLE_VP_VTL);

        tracing::debug!("enabled vtl 1 on vp {}", vp_index);

        Ok(())
    }

    pub fn hcvm_get_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hvdef::HvRegisterName],
        output: &mut [hvdef::HvRegisterValue],
    ) -> hvdef::HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::AccessDenied, 0));
        }

        let vtl = self
            .target_vtl_no_higher(vtl.unwrap_or_else(|| self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;

        for (i, (&name, output)) in zip(registers, output).enumerate() {
            *output = self.get_vp_register(name, vtl).map_err(|e| (e, i))?;
        }

        Ok(())
    }

    fn get_vp_register(
        &mut self,
        name: hvdef::HvRegisterName,
        vtl: GuestVtl,
    ) -> HvResult<hvdef::HvRegisterValue> {
        match name.into() {
            hvdef::HvX64RegisterName::VsmCodePageOffsets => Ok(u64::from(
                self.vp.hv[vtl]
                    .as_ref()
                    .expect("hv emulator exists for cvm")
                    .vsm_code_page_offsets(true),
            )
            .into()),
            hvdef::HvX64RegisterName::VsmCapabilities => Ok(u64::from(
                hvdef::HvRegisterVsmCapabilities::new().with_deny_lower_vtl_startup(true),
            )
            .into()),
            _ => Err(HvError::InvalidParameter),
        }
    }

    fn retarget_physical_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        vector: u32,
        multicast: bool,
        target_processors: &[u32],
    ) -> HvResult<()> {
        self.vp.partition.hcl.retarget_device_interrupt(
            device_id,
            hvdef::hypercall::InterruptEntry {
                source: hvdef::hypercall::HvInterruptSource::MSI,
                rsvd: 0,
                data: [address as u32, data],
            },
            vector,
            multicast,
            target_processors,
        )
    }

    pub fn hcvm_retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        vector: u32,
        multicast: bool,
        target_processors: &[u32],
    ) -> HvResult<()> {
        // It is unknown whether the interrupt is physical or virtual, so try both. Note that the
        // actual response from the hypervisor can't really be trusted so:
        // 1. Always invoke the virtual interrupt retargeting.
        // 2. A failure from the physical interrupt retargeting is not necessarily a sign of a
        // malicious hypervisor or a buggy guest, since the target could simply be a virtual one.
        let hv_result = self.retarget_physical_interrupt(
            device_id,
            address,
            data,
            vector,
            multicast,
            target_processors,
        );
        let virtual_result = self.retarget_virtual_interrupt(
            device_id,
            address,
            data,
            vector,
            multicast,
            target_processors,
        );
        hv_result.or(virtual_result)
    }

    pub fn hcvm_validate_flush_inputs(
        &mut self,
        processor_set: &[u32],
        flags: HvFlushFlags,
        allow_extended_ranges: bool,
    ) -> HvResult<()> {
        let valid_flags = HvFlushFlags::new()
            .with_all_processors(true)
            .with_all_virtual_address_spaces(true)
            .with_non_global_mappings_only(true)
            .with_use_extended_range_format(allow_extended_ranges);

        if u64::from(flags) & !u64::from(valid_flags) != 0 {
            return Err(HvError::InvalidParameter);
        }
        if processor_set.is_empty() && !flags.all_processors() {
            return Err(HvError::InvalidParameter);
        }
        // TODO should we check the all_virtual_address_spaces flag? we don't check this flag or the address space input arg anywhere in the hcl
        Ok(())
    }

    pub fn hcvm_is_vtl_call_allowed(&self) -> bool {
        tracing::trace!("checking if vtl call is allowed");

        // Only allowed from VTL 0
        if self.intercepted_vtl != GuestVtl::Vtl0 {
            false
        } else if !*self.vp.inner.hcvm_vtl1_enabled.lock() {
            // VTL 1 must be enabled on the vp
            false
        } else {
            true
        }
    }

    pub fn hcvm_vtl_call(&mut self) {
        tracing::trace!("handling vtl call");

        self.vp.switch_vtl(self.intercepted_vtl, GuestVtl::Vtl1);
        self.vp.backing.cvm_state_mut().exit_vtl = GuestVtl::Vtl1;

        // TODO GUEST_VSM: Force reevaluation of the VTL 1 APIC in case delivery of
        // low-priority interrupts was suppressed while in VTL 0.

        // TODO GUEST_VSM: Track which VTLs are runnable and mark VTL as runnable
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::SetVpRegisters
    for UhHypercallHandler<'_, '_, T, B>
{
    fn set_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hvdef::hypercall::HvRegisterAssoc],
    ) -> hvdef::HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::InvalidVpIndex, 0));
        }

        let target_vtl = vtl
            .map_or_else(|| Ok(self.intercepted_vtl), |vtl| vtl.try_into())
            .map_err(|_| (HvError::InvalidParameter, 0))?;

        for (i, reg) in registers.iter().enumerate() {
            if reg.name == hvdef::HvX64RegisterName::VsmPartitionConfig.into() {
                let value = HvRegisterVsmPartitionConfig::from(reg.value.as_u64());
                self.vp
                    .set_vsm_partition_config(value, target_vtl)
                    .map_err(|e| (e, i))?;
            } else {
                return Err((HvError::InvalidParameter, i));
            }
        }

        Ok(())
    }
}

impl<B: HardwareIsolatedBacking> UhProcessor<'_, B> {
    fn set_vsm_partition_config(
        &mut self,
        value: HvRegisterVsmPartitionConfig,
        vtl: GuestVtl,
    ) -> Result<(), HvError> {
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        assert!(self.partition.isolation.is_isolated());

        // Features currently supported by openhcl.
        let allowed_bits = HvRegisterVsmPartitionConfig::new()
            .with_enable_vtl_protection(true)
            .with_default_vtl_protection_mask(0xf)
            .with_zero_memory_on_reset(true)
            .with_deny_lower_vtl_startup(true);

        if (!u64::from(allowed_bits) & u64::from(value)) != 0 {
            return Err(HvError::InvalidRegisterValue);
        }

        // VTL 1 mut be enabled already.
        let mut guest_vsm_lock = self.partition.guest_vsm.write();
        let guest_vsm = guest_vsm_lock
            .get_vtl1_mut()
            .ok_or(HvError::InvalidVtlState)?;
        let guest_vsm_inner = guest_vsm.inner.get_hardware_cvm_mut().unwrap();

        let protections = HvMapGpaFlags::from(value.default_vtl_protection_mask() as u32);

        // VTL protection cannot be disabled once enabled.
        if !value.enable_vtl_protection() && guest_vsm.enable_vtl_protection {
            return Err(HvError::InvalidRegisterValue);
        }

        if !validate_vtl_gpa_flags(
            protections,
            guest_vsm_inner.mbec_enabled,
            guest_vsm_inner.shadow_supervisor_stack_enabled,
        ) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Default VTL protection mask must include read and write.
        if !(protections.readable() && protections.writable()) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Protections given to set_vsm_partition_config actually apply to VTLs lower
        // than the VTL specified as an argument for hardware CVMs.
        let targeted_vtl = GuestVtl::Vtl0;

        let protector = self
            .partition
            .isolated_memory_protector
            .as_ref()
            .expect("isolated memory protector must exist for a CVM");

        // Don't allow changing existing protections once vtl protection is enabled
        if guest_vsm.enable_vtl_protection {
            let current_protections = protector.default_vtl0_protections();
            if protections != current_protections {
                return Err(HvError::InvalidRegisterValue);
            }
        }

        protector.change_default_vtl_protections(targeted_vtl, protections)?;

        // TODO GUEST VSM: actually use the enable_vtl_protection value
        guest_vsm.enable_vtl_protection = value.enable_vtl_protection();

        // Note: Zero memory on reset will happen regardless of this value,
        // since reset that involves resetting from UEFI isn't supported, and
        // the partition will get torn down and reconstructed by the host.
        guest_vsm_inner.zero_memory_on_reset = value.zero_memory_on_reset();
        guest_vsm_inner.deny_lower_vtl_startup = value.deny_lower_vtl_startup();

        Ok(())
    }
}

pub(crate) struct XsetbvExitInput {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub cr4: u64,
    pub cpl: u8,
}

/// Validates registers are in the correct states during a xsetbv exit, and return
/// the new xfem value if everything's valid.
pub(crate) fn validate_xsetbv_exit(input: XsetbvExitInput) -> Option<u64> {
    let XsetbvExitInput {
        rax,
        rcx,
        rdx,
        cr4,
        cpl,
    } = input;

    if rcx != 0 {
        tracelimit::warn_ratelimited!(rcx, "xsetbv exit: rcx is not set to 0");
        return None;
    }

    if cpl != 0 {
        tracelimit::warn_ratelimited!(cpl, "xsetbv exit: invalid cpl");
        return None;
    }

    let osxsave_flag = cr4 & x86defs::X64_CR4_OSXSAVE;
    if osxsave_flag == 0 {
        tracelimit::warn_ratelimited!(cr4, "xsetbv exit: cr4 osxsave not set");
        return None;
    }

    let xfem = (rdx << 32) | (rax & 0xffffffff);

    if (xfem & x86defs::xsave::XFEATURE_X87) == 0 {
        tracelimit::warn_ratelimited!(xfem, "xsetbv exit: xfem legacy x87 bit not set");
        return None;
    }

    Some(xfem)
}
