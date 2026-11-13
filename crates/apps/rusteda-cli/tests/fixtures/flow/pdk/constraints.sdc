current_design mini
create_clock -name clk_i -period 10.0000 [get_ports {clk_i}]
set_clock_transition 0.1500 [get_clocks {clk_i}]
set_clock_uncertainty 0.0500 clk_i
set_propagated_clock [get_clocks {clk_i}]
set_input_delay 2.0000 -max -clock [get_clocks {clk_i}] -add_delay [get_ports {rst_ni}]
set_input_delay 2.0000 -max -clock [get_clocks {clk_i}] -add_delay [get_ports {a_i}]
set_input_delay 2.0000 -max -clock [get_clocks {clk_i}] -add_delay [get_ports {b_i}]
set_driving_cell -lib_cell BUFX4 -pin {Y} -input_transition_rise 0.0000 -input_transition_fall 0.0000 [get_ports {clk_i}]
set_driving_cell -lib_cell BUFX4 -pin {Y} -input_transition_rise 0.0000 -input_transition_fall 0.0000 [get_ports {rst_ni}]
set_driving_cell -lib_cell BUFX4 -pin {Y} -input_transition_rise 0.0000 -input_transition_fall 0.0000 [get_ports {a_i}]
set_driving_cell -lib_cell BUFX4 -pin {Y} -input_transition_rise 0.0000 -input_transition_fall 0.0000 [get_ports {b_i}]
set_load -pin_load 0.0060 [get_ports {q_o}]
set_max_fanout 10.0000 [current_design]
