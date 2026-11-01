// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
//! At most four pins and six nets per test; no search or path enumeration.
use rusteda_core::netlist::{Conn, PinDir, PinRef};
use rusteda_formats::verilog::netlist::read_structural;
#[test]
fn library_bus_order_is_preserved() {
    for (order, first, second) in [([1, 0], "D[1]", "D[0]"), ([0, 1], "D[0]", "D[1]")] {
        let pins = |_: &str| {
            Some(
                order
                    .iter()
                    .map(|i| (format!("D[{i}]"), PinDir::Input))
                    .collect(),
            )
        };
        let g = read_structural(
            "module t(input [3:2] a); RAM r(.D(a)); endmodule",
            "t.v",
            &pins,
        )
        .unwrap();
        let instance = g.find_instance("r").unwrap();
        for (pin, bit) in [(first, 3), (second, 2)] {
            let pin = g.find_pin(instance, PinRef::Name(pin)).unwrap();
            assert_eq!(
                g.pin(pin).unwrap().conn,
                Some(Conn::Net(g.find_net("a", Some(bit)).unwrap()))
            );
        }
    }
}
#[test]
fn mixed_scalar_and_bus_connection_is_rejected() {
    let pins = |_: &str| {
        Some(vec![
            ("D[1]".into(), PinDir::Input),
            ("D[0]".into(), PinDir::Input),
        ])
    };
    assert!(
        read_structural(
            "module t(input [1:0] a); RAM r(.D(a), .\\D[0] (a[0])); endmodule",
            "t.v",
            &pins
        )
        .is_err()
    );
    assert!(read_structural("module t(input a); RAM r(.D(a)); endmodule", "t.v", &pins).is_err());
}
