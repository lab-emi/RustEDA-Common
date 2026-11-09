// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
//! Small projection fixtures; <1MiB tables and no path enumeration.
use super::*;
use rusteda_core::netlist::{Conn, GateNetlist, PinDir, PortDir};
use serde_json::Value;

fn graph() -> GateNetlist {
    let mut g = GateNetlist::new("top").unwrap();
    g.add_port("a", PortDir::Input, Some((0, 3))).unwrap();
    g.add_port("y", PortDir::Output, Some((7, 4))).unwrap();
    let mut pins = vec![("S[0]".to_string(), PinDir::Input)];
    pins.extend((0..4).map(|i| (format!("A[{i}]"), PinDir::Input)));
    pins.extend((4..8).map(|i| (format!("Q[{i}]"), PinDir::Output)));
    let refs: Vec<_> = pins.iter().map(|(s, d)| (s.as_str(), *d)).collect();
    let c = g.add_cell_type("MEM", &refs).unwrap();
    let u = g.add_instance("u", c).unwrap();
    for i in 0..4 {
        let a = format!("A[{i}]");
        let q = format!("Q[{}]", i + 4);
        g.connect(u, a.as_str(), Conn::Net(g.find_net("a", Some(i)).unwrap()))
            .unwrap();
        g.connect(
            u,
            q.as_str(),
            Conn::Net(g.find_net("y", Some(i + 4)).unwrap()),
        )
        .unwrap();
    }
    let one = g.add_net(Some("one"), None).unwrap();
    let alias = g.add_net(Some("alias"), None).unwrap();
    g.add_assign(one, Conn::Const(true)).unwrap();
    g.add_assign(alias, Conn::Net(one)).unwrap();
    g.connect(u, "S[0]", Conn::Net(alias)).unwrap();
    g
}
fn schema() -> CellBusSchemas {
    [(
        "MEM".into(),
        [("A".into(), [0, 3]), ("Q".into(), [7, 4])].into(),
    )]
    .into()
}
fn render(g: &GateNetlist, schema: &CellBusSchemas) -> Value {
    let f = g.freeze();
    f.validate().unwrap();
    let plan = Plan::new(&f, schema, Limits::default()).unwrap();
    let mut out = Vec::new();
    plan.write(&mut out).unwrap();
    serde_json::from_slice(&out).unwrap()
}
#[test]
fn original_ranges_alias_constants_and_literal_pins_are_preserved() {
    let v = render(&graph(), &schema());
    let top = &v["modules"]["top"];
    assert_eq!(top["ports"]["a"]["offset"], 0);
    assert_eq!(top["ports"]["a"]["upto"], 1);
    assert_eq!(top["ports"]["y"]["offset"], 4);
    assert_eq!(top["ports"]["y"]["upto"], 0);
    assert_eq!(
        top["cells"]["u"]["connections"]["A"],
        top["ports"]["a"]["bits"]
    );
    assert_eq!(
        top["cells"]["u"]["connections"]["Q"],
        top["ports"]["y"]["bits"]
    );
    assert_eq!(top["cells"]["u"]["connections"]["S[0]"][0], "1");
    assert_eq!(top["netnames"]["alias"]["bits"][0], "1");
    assert!(v["modules"]["MEM"]["ports"]["S[0]"].get("is_bus").is_none());
    let scalar = render(&graph(), &CellBusSchemas::new());
    assert!(
        scalar["modules"]["top"]["cells"]["u"]["connections"]
            .get("A[0]")
            .is_some()
    );
    assert!(
        scalar["modules"]["top"]["cells"]["u"]["connections"]
            .get("A")
            .is_none()
    );
}
#[test]
fn one_bit_bus_and_open_pins_remain_explicit() {
    let mut g = GateNetlist::new("top").unwrap();
    g.add_port("a", PortDir::Input, Some((5, 5))).unwrap();
    let c = g
        .add_cell_type("M", &[("A[5]", PinDir::Input), ("Y", PinDir::Output)])
        .unwrap();
    let u = g.add_instance("u", c).unwrap();
    g.connect(u, "A[5]", Conn::Net(g.find_net("a", Some(5)).unwrap()))
        .unwrap();
    let schema = [("M".into(), [("A".into(), [5, 5])].into())].into();
    let v = render(&g, &schema);
    assert_eq!(v["modules"]["top"]["ports"]["a"]["is_bus"], 1);
    assert_eq!(v["modules"]["M"]["ports"]["A"]["offset"], 5);
    assert_eq!(
        v["modules"]["top"]["cells"]["u"]["connections"]["Y"][0], "x",
        "unconnected must never become zero"
    );
}
#[test]
fn budgets_contradictory_aliases_and_invalid_schemas_fail_explicitly() {
    let g = graph();
    let f = g.freeze();
    f.validate().unwrap();
    for limits in [
        Limits {
            aux_bytes: 1,
            ..Limits::default()
        },
        Limits {
            work: 1,
            ..Limits::default()
        },
    ] {
        assert!(Plan::new(&f, &schema(), limits).is_err());
    }
    let missing = [("MEM".into(), [("A".into(), [4, 0])].into())].into();
    assert!(Plan::new(&f, &missing, Limits::default()).is_err());
    let mixed = [(
        "MEM".into(),
        [("A".into(), [0, 3]), ("S".into(), [0, 0])].into(),
    )]
    .into();
    let mut f2 = f.clone();
    f2.cell_pin_dir.to_mut()[1] = 1;
    f2.validate().unwrap();
    assert!(Plan::new(&f2, &mixed, Limits::default()).is_err());
    let mut g = GateNetlist::new("bad").unwrap();
    let n = g.add_net(Some("n"), None).unwrap();
    g.add_assign(n, Conn::Const(false)).unwrap();
    g.add_assign(n, Conn::Const(true)).unwrap();
    let f = g.freeze();
    f.validate().unwrap();
    assert!(Plan::new(&f, &CellBusSchemas::new(), Limits::default()).is_err());
}
