# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 RustEDA contributors
"""Stands for a helper of the RustEDA Ibex recipe in the context of this fixture, which pins it
by hash. Only reference.py runs it: it then loads the real helper named by RUSTEDA_REFERENCE_RECIPE."""
import os

_real = os.path.join(os.environ['RUSTEDA_REFERENCE_RECIPE'], 'native_frontend_inputs.py')
with open(_real) as _stream:
    exec(compile(_stream.read(), __file__, 'exec'))
