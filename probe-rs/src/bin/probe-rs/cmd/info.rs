use std::fmt::Write;

use anyhow::{anyhow, Result};
use probe_rs::{
    architecture::{
        arm::{
            ap::{GenericAp, MemoryAp},
            armv6m::Demcr,
            component::Scs,
            dp::{DPIDR, TARGETID},
            memory::{Component, CoresightComponent, PeripheralType},
            sequences::DefaultArmSequence,
            ApAddress, ApInformation, ArmProbeInterface, DpAddress, MemoryApInformation, Register,
        },
        riscv::communication_interface::RiscvCommunicationInterface,
        xtensa::communication_interface::XtensaCommunicationInterface,
    },
    Lister, MemoryMappedRegister, Probe, WireProtocol,
};
use termtree::Tree;

use crate::util::common_options::ProbeOptions;

#[derive(clap::Parser)]
pub struct Cmd {
    #[clap(flatten)]
    common: ProbeOptions,
    /// SWD Multidrop target selection value
    ///
    /// If provided, this value is written into the debug port TARGETSEL register
    /// when connecting. This is required for targets using SWD multidrop
    #[arg(long, value_parser = parse_hex)]
    target_sel: Option<u32>,
}

// Clippy doesn't like `from_str_radix` with radix 10, but I prefer the symmetry`
// with the hex case.
#[allow(clippy::from_str_radix_10)]
fn parse_hex(src: &str) -> Result<u32, std::num::ParseIntError> {
    if src.starts_with("0x") {
        u32::from_str_radix(src.trim_start_matches("0x"), 16)
    } else {
        u32::from_str_radix(src, 10)
    }
}

impl Cmd {
    pub fn run(self, lister: &Lister) -> anyhow::Result<()> {
        let probe_options = self.common.load()?;
        let mut probe = probe_options.attach_probe(lister)?;

        let protocols = if let Some(protocol) = probe_options.protocol() {
            vec![protocol]
        } else {
            vec![WireProtocol::Jtag, WireProtocol::Swd]
        };

        for protocol in protocols {
            println!("Probing target via {protocol}");
            println!();

            let (new_probe, result) = try_show_info(
                probe,
                protocol,
                probe_options.connect_under_reset(),
                self.target_sel,
            );

            probe = new_probe;

            probe.detach()?;

            if let Err(e) = result {
                println!("Error identifying target using protocol {protocol}: {e}");
            }

            println!();
        }

        Ok(())
    }
}

fn try_show_info(
    mut probe: Probe,
    protocol: WireProtocol,
    connect_under_reset: bool,
    target_sel: Option<u32>,
) -> (Probe, Result<()>) {
    if let Err(e) = probe.select_protocol(protocol) {
        return (probe, Err(e.into()));
    }

    let attach_result = if connect_under_reset {
        probe.attach_to_unspecified_under_reset()
    } else {
        probe.attach_to_unspecified()
    };

    if let Err(e) = attach_result {
        return (probe, Err(e.into()));
    }

    let dp = target_sel.map(DpAddress::Multidrop).unwrap_or_default();

    let mut probe = probe;

    if probe.has_arm_interface() {
        log::debug!("Trying to show ARM chip information");
        match probe.try_into_arm_interface() {
            Ok(interface) => {
                match interface.initialize(DefaultArmSequence::create(), dp) {
                    Ok(mut interface) => {
                        if let Err(e) = show_arm_info(&mut *interface, dp) {
                            // Log error?
                            println!("Error showing ARM chip information: {:?}", anyhow!(e));
                        }

                        probe = interface.close();
                    }
                    Err((interface, e)) => {
                        println!("Error showing ARM chip information: {:?}", anyhow!(e));

                        probe = interface.close();
                    }
                }
            }
            Err((interface_probe, e)) => {
                println!("Error showing ARM chip information: {:?}", anyhow!(e));
                probe = interface_probe;
            }
        }
    } else {
        println!("No DAP interface was found on the connected probe. ARM-specific information cannot be printed.");
    }

    // This check is a bit weird, but `try_into_riscv_interface` will try to switch the protocol to JTAG.
    // If the current protocol we want to use is SWD, we have avoid this.
    if probe.has_riscv_interface() && protocol == WireProtocol::Jtag {
        log::debug!("Trying to show RISC-V chip information");
        match probe.try_into_riscv_interface() {
            Ok(mut interface) => {
                if let Err(e) = show_riscv_info(&mut interface) {
                    println!("Error showing RISC-V chip information: {:?}", anyhow!(e));
                }

                probe = interface.close();
            }
            Err((interface_probe, e)) => {
                println!("Error while reading RISC-V info: {:?}", anyhow!(e));
                probe = interface_probe;
            }
        }
    } else {
        if protocol == WireProtocol::Swd {
            println!(
                "Debugging RISC-V targets over SWD is not supported. For these targets, JTAG is the only supported protocol. RISC-V specific information cannot be printed."
            );
        } else {
            println!(
                "Unable to debug RISC-V targets using the current probe. RISC-V specific information cannot be printed."
            );
        }
    }

    // This check is a bit weird, but `try_into_xtensa_interface` will try to switch the protocol to JTAG.
    // If the current protocol we want to use is SWD, we have avoid this.
    if probe.has_xtensa_interface() && protocol == WireProtocol::Jtag {
        log::debug!("Trying to show Xtensa chip information");
        match probe.try_into_xtensa_interface() {
            Ok(mut interface) => {
                if let Err(e) = show_xtensa_info(&mut interface) {
                    println!("Error showing Xtensa chip information: {:?}", anyhow!(e));
                }

                probe = interface.close();
            }
            Err((interface_probe, e)) => {
                println!("Error showing Xtensa chip information: {:?}", anyhow!(e));
                probe = interface_probe;
            }
        }
    } else {
        if protocol == WireProtocol::Swd {
            println!(
                "Debugging Xtensa targets over SWD is not supported. For these targets, JTAG is the only supported protocol. Xtensa specific information cannot be printed."
            );
        } else {
            println!(
            "Unable to debug Xtensa targets using the current probe. Xtensa specific information cannot be printed."
        );
        }
    }

    (probe, Ok(()))
}

fn show_arm_info(interface: &mut dyn ArmProbeInterface, dp: DpAddress) -> Result<()> {
    let dp_info = interface.read_raw_dp_register(dp, DPIDR::ADDRESS)?;
    let dp_info = DPIDR(dp_info);

    let mut dp_node = String::new();

    write!(dp_node, "Debug Port: Version {}", dp_info.version())?;

    if dp_info.min() {
        write!(dp_node, ", MINDP")?;
    }

    let jep_code = jep106::JEP106Code::new(dp_info.jep_cc(), dp_info.jep_id());

    if dp_info.version() == 2 {
        let target_id = interface.read_raw_dp_register(dp, TARGETID::ADDRESS)?;

        let target_id = TARGETID(target_id);

        let part_no = target_id.tpartno();
        let revision = target_id.trevision();

        let designer_id = target_id.tdesigner();

        let cc = (designer_id >> 7) as u8;
        let id = (designer_id & 0x7f) as u8;

        let designer = jep106::JEP106Code::new(cc, id);

        write!(
            dp_node,
            ", Designer: {}",
            designer.get().unwrap_or("<unknown>")
        )?;
        write!(dp_node, ", Part: {part_no:#x}")?;
        write!(dp_node, ", Revision: {revision:#x}")?;
    } else {
        write!(
            dp_node,
            ", DP Designer: {}",
            jep_code.get().unwrap_or("<unknown>")
        )?;
    }

    let mut tree = Tree::new(dp_node);

    let num_access_ports = interface.num_access_ports(dp)?;

    for ap_index in 0..num_access_ports {
        let ap = ApAddress {
            ap: ap_index as u8,
            dp,
        };
        let access_port = GenericAp::new(ap);

        let ap_information = interface.ap_information(access_port)?;

        match ap_information {
            ApInformation::MemoryAp(MemoryApInformation {
                debug_base_address,
                address,
                device_enabled,
                ..
            }) => {
                let mut ap_nodes = Tree::new(format!("{} MemoryAP", address.ap));

                if *device_enabled {
                    match handle_memory_ap(access_port.into(), *debug_base_address, interface) {
                        Ok(component_tree) => ap_nodes.push(component_tree),
                        Err(e) => ap_nodes.push(format!("Error during access: {e}")),
                    };
                } else {
                    ap_nodes.push("Access disabled".to_string());
                }

                tree.push(ap_nodes);
            }

            ApInformation::Other { address, idr } => {
                let designer = idr.DESIGNER;

                let cc = (designer >> 7) as u8;
                let id = (designer & 0x7f) as u8;

                let jep = jep106::JEP106Code::new(cc, id);

                let ap_type = if designer == 0x43b {
                    format!("{:?}", idr.TYPE)
                } else {
                    format!("{:#x}", idr.TYPE as u8)
                };

                tree.push(format!(
                    "{} Unknown AP (Designer: {}, Class: {:?}, Type: {}, Variant: {:#x}, Revision: {:#x})",
                    address.ap,
                    jep.get().unwrap_or("<unknown>"),
                    idr.CLASS,
                    ap_type,
                    idr.VARIANT,
                    idr.REVISION
                ));
            }
        }
    }

    println!("ARM Chip:");
    println!("{tree}");

    Ok(())
}

fn handle_memory_ap(
    access_port: MemoryAp,
    base_address: u64,
    interface: &mut dyn ArmProbeInterface,
) -> Result<Tree<String>, anyhow::Error> {
    let component = {
        let mut memory = interface.memory_interface(access_port)?;
        let mut demcr = Demcr(memory.read_word_32(Demcr::get_mmio_address())?);
        demcr.set_dwtena(true);
        memory.write_word_32(Demcr::get_mmio_address(), demcr.into())?;
        Component::try_parse(&mut *memory, base_address)?
    };
    let component_tree = coresight_component_tree(interface, component, access_port)?;

    Ok(component_tree)
}

fn coresight_component_tree(
    interface: &mut dyn ArmProbeInterface,
    component: Component,
    access_port: MemoryAp,
) -> Result<Tree<String>> {
    let tree = match &component {
        Component::GenericVerificationComponent(_) => Tree::new("Generic".to_string()),
        Component::Class1RomTable(_, table) => {
            let mut rom_table = Tree::new("ROM Table (Class 1)".to_string());

            for entry in table.entries() {
                let component = entry.component().clone();

                rom_table.push(coresight_component_tree(interface, component, access_port)?);
            }

            rom_table
        }
        Component::CoresightComponent(id) => {
            let peripheral_id = id.peripheral_id();

            let component_description = if let Some(part_info) = peripheral_id.determine_part() {
                format!("{: <15} (Coresight Component)", part_info.name())
            } else {
                format!(
                    "Coresight Component, Part: {:#06x}, Devtype: {:#04x}, Archid: {:#06x}, Designer: {}",
                    peripheral_id.part(),
                    peripheral_id.dev_type(),
                    peripheral_id.arch_id(),
                    peripheral_id
                        .jep106()
                        .and_then(|j| j.get())
                        .unwrap_or("<unknown>"),
                )
            };

            Tree::new(component_description)
        }

        Component::PeripheralTestBlock(_) => Tree::new("Peripheral test block".to_string()),
        Component::GenericIPComponent(id) => {
            let peripheral_id = id.peripheral_id();

            let desc = if let Some(part_desc) = peripheral_id.determine_part() {
                format!("{: <15} (Generic IP component)", part_desc.name())
            } else {
                "Generic IP component".to_string()
            };

            let mut tree = Tree::new(desc);

            if peripheral_id.is_of_type(PeripheralType::Scs) {
                let cc = &CoresightComponent::new(component, access_port);
                let scs = &mut Scs::new(interface, cc);
                let cpu_tree = cpu_info_tree(scs)?;

                tree.push(cpu_tree);
            }

            tree
        }

        Component::CoreLinkOrPrimeCellOrSystemComponent(_) => {
            Tree::new("Core Link / Prime Cell / System component".to_string())
        }
    };

    Ok(tree)
}

fn cpu_info_tree(scs: &mut Scs) -> Result<Tree<String>> {
    let mut tree = Tree::new("CPUID".into());

    let cpuid = scs.cpuid()?;

    let implementer = cpuid.implementer();
    let implementer = if implementer == 0x41 {
        "ARM Ltd".into()
    } else {
        implementer.to_string()
    };

    tree.push(format!("IMPLEMENTER: {implementer}"));
    tree.push(format!("VARIANT: {}", cpuid.variant()));
    tree.push(format!("PARTNO: {}", cpuid.partno()));
    tree.push(format!("REVISION: {}", cpuid.revision()));

    Ok(tree)
}

fn show_riscv_info(interface: &mut RiscvCommunicationInterface) -> Result<()> {
    let idcode = interface.read_idcode()?;

    print_idcode_info("RISC-V", idcode);

    Ok(())
}

fn show_xtensa_info(interface: &mut XtensaCommunicationInterface) -> Result<()> {
    let idcode = interface.read_idcode()?;

    print_idcode_info("Xtensa", idcode);

    Ok(())
}

fn print_idcode_info(architecture: &str, idcode: u32) {
    let version = (idcode >> 28) & 0xf;
    let part_number = (idcode >> 12) & 0xffff;
    let manufacturer_id = (idcode >> 1) & 0x7ff;

    let jep_cc = (manufacturer_id >> 7) & 0xf;
    let jep_id = manufacturer_id & 0x7f;

    let jep_id = jep106::JEP106Code::new(jep_cc as u8, jep_id as u8);

    println!("{architecture} Chip:");
    println!("  IDCODE: {idcode:010x}");
    println!("    Version:      {version}");
    println!("    Part:         {part_number}");
    println!("    Manufacturer: {manufacturer_id} ({jep_id})");
}
