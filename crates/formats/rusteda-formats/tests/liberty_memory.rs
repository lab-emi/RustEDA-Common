// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors
use rusteda_formats::liberty::read_text_diags;
#[test]
fn unquoted_memory_slice_keeps_each_timed_pin() {
    let source = r#"library(M) {
      type(T) { base_type:array; data_type:bit; bit_from:1; bit_to:0; bit_width:2; }
      cell(RAM) { area:2; memory(){ type:ram; address_width:1; word_width:2; }
        pin(CLK){direction:input; clock:true;}
        bus(D) {bus_type:T; direction:input; memory_write(){address:A;clocked_on:CLK;}
          pin(D[1:0]) {capacitance:0.1; timing(){ related_pin:CLK;timing_type:setup_rising; rise_constraint(scalar){values(0.2);}}}}
        bus(Q) {bus_type:T; direction:output; memory_read(){address:A;}
          pin(Q[1:0]) {timing(){related_pin:CLK;timing_type:rising_edge;cell_rise(scalar){values(0.5);}}}}
      }}"#;
    let (lib, diags) = read_text_diags(source, "memory.lib").unwrap();
    let cell = &lib.cells[0];
    assert!(cell.is_macro_cell);
    assert_eq!(cell.pins.len(), 5);
    for name in ["D[0]", "D[1]", "Q[0]", "Q[1]"] {
        assert_eq!(cell.pin(name).unwrap().arcs.len(), 1);
    }
    assert!(diags.iter().any(|d| format!("{d:?}").contains("memory")));
}
#[test]
fn huge_bus_decl_is_rejected_before_expansion() {
    let source = "library(M){type(T){bit_from:2147483647;bit_to:-2147483648;}cell(C){bus(A){bus_type:T;direction:input;}}}";
    let (lib, diags) = read_text_diags(source, "huge.lib").unwrap();
    assert!(lib.cells[0].pins.is_empty());
    assert!(!diags.is_empty());
}
