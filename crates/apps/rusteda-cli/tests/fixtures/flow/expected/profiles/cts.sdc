# CTS-only literal projection; full original SDC remains mandatory for STA.
create_clock -name clk_i -period 10.0 [get_ports {clk_i}]
set_clock_transition 0.15 [get_clocks {clk_i}]
set_clock_uncertainty 0.05 [get_clocks {clk_i}]
set_propagated_clock [get_clocks {clk_i}]
set_max_fanout 10.0 [current_design]
