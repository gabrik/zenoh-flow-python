//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use async_trait::async_trait;
use pyo3::types::{PyDict, PyString};
use pyo3::{prelude::*, types::PyModule};
use pyo3_asyncio::TaskLocals;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use zenoh_flow::prelude::*;
use zenoh_flow_python_commons::{
    configuration_into_py, context_into_py, from_pyerr_to_zferr, Input as PyInput,
    Output as PyOutput, PythonState,
};

#[cfg(target_family = "unix")]
use libloading::os::unix::Library;
#[cfg(target_family = "windows")]
use libloading::Library;

#[cfg(target_family = "unix")]
static LOAD_FLAGS: std::os::raw::c_int =
    libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_GLOBAL;

#[export_operator]
#[derive(Clone)]
struct PyOperator {
    state: Arc<PythonState>,
    _lib: Arc<Library>,
}

#[async_trait]
impl Operator for PyOperator {
    async fn new(
        ctx: Context,
        configuration: Option<Configuration>,
        mut inputs: Inputs,
        mut outputs: Outputs,
    ) -> Result<Self> {
        let lib = Arc::new(load_self()?);

        pyo3::prepare_freethreaded_python();

        let state = Arc::new(Python::with_gil(|py| {
            match configuration {
                Some(configuration) => {
                    // Unwrapping configuration
                    let script_file_path = Path::new(
                        configuration["python-script"]
                            .as_str()
                            .ok_or_else(|| zferror!(ErrorKind::InvalidState))?,
                    );
                    let mut config = configuration.clone();
                    config["python-script"].take();
                    let py_config = config["configuration"].take();

                    // Convert configuration to Python
                    let py_config = configuration_into_py(py, py_config)
                        .map_err(|e| from_pyerr_to_zferr(e, &py))?;

                    // Load Python code
                    let code = read_file(script_file_path)?;
                    let module =
                        PyModule::from_code(py, &code, &script_file_path.to_string_lossy(), "op")
                            .map_err(|e| from_pyerr_to_zferr(e, &py))?;

                    // Getting the correct python module
                    let op_class = module
                        .call_method0("register")
                        .map_err(|e| from_pyerr_to_zferr(e, &py))?;

                    let py_receivers = PyDict::new(py);

                    let inputs_ids = inputs.keys().cloned().collect::<Vec<_>>();
                    for id in &inputs_ids {
                        let input = inputs
                            .take_raw(id)
                            .ok_or_else(|| zferror!(ErrorKind::MissingInput(id.to_string())))?;
                        let pyo3_rx = PyInput::from(input);
                        py_receivers
                            .set_item(PyString::new(py, id), &pyo3_rx.into_py(py))
                            .map_err(|e| from_pyerr_to_zferr(e, &py))?;
                    }

                    let py_senders = PyDict::new(py);

                    let outputs_ids = outputs.keys().cloned().collect::<Vec<_>>();
                    for id in &outputs_ids {
                        let output = outputs
                            .take_raw(id)
                            .ok_or_else(|| zferror!(ErrorKind::MissingOutput(id.to_string())))?;
                        let pyo3_tx = PyOutput::from(output);
                        py_senders
                            .set_item(PyString::new(py, id), &pyo3_tx.into_py(py))
                            .map_err(|e| from_pyerr_to_zferr(e, &py))?;
                    }

                    // Setting asyncio event loop
                    let asyncio = py.import("asyncio").unwrap();

                    let event_loop = asyncio.call_method0("new_event_loop").unwrap();
                    asyncio
                        .call_method1("set_event_loop", (event_loop,))
                        .unwrap();
                    let event_loop_hdl = Arc::new(PyObject::from(event_loop));
                    let asyncio_module = Arc::new(PyObject::from(asyncio));
                    let py_ctx =
                        context_into_py(&py, &ctx).map_err(|e| from_pyerr_to_zferr(e, &py))?;

                    // Initialize Python Object
                    let py_op: PyObject = op_class
                        .call1((py_ctx, py_config, py_receivers, py_senders))
                        .map_err(|e| from_pyerr_to_zferr(e, &py))?
                        .into();

                    let py_state = PythonState {
                        module: Arc::new(op_class.into()),
                        py_state: Arc::new(py_op),
                        event_loop: event_loop_hdl,
                        asyncio_module,
                    };

                    Ok(py_state)
                }
                None => Err(zferror!(ErrorKind::InvalidState)),
            }
        })?);

        Ok(Self { _lib: lib, state })
    }
}

#[async_trait]
impl Node for PyOperator {
    async fn iteration(&self) -> Result<()> {
        Python::with_gil(|py| {
            let op_class = self.state.py_state.cast_as::<PyAny>(py)?;

            let event_loop = self.state.event_loop.cast_as::<PyAny>(py)?;

            let task_locals = TaskLocals::new(event_loop);

            let py_future = op_class.call_method0("iteration")?;

            let fut = pyo3_asyncio::into_future_with_locals(&task_locals, py_future)?;
            pyo3_asyncio::async_std::run_until_complete(event_loop, fut)
        })
        .map_err(|e| Python::with_gil(|py| from_pyerr_to_zferr(e, &py)))?;
        Ok(())
    }
}

fn load_self() -> Result<Library> {
    // Very dirty hack! We explicit load the python library!
    let config = pyo3_build_config::get();
    match &config.lib_name {
        Some(name) => {
        log::trace!("Python Operator Wrapper loading Python {}", name);
            let lib_name = libloading::library_filename(name.clone());
            unsafe {
                #[cfg(target_family = "unix")]
                let lib = Library::open(Some(lib_name), LOAD_FLAGS).map_err(|_| zferror!(ErrorKind::NotFound))?;

                #[cfg(target_family = "windows")]
                let lib = Library::new(lib_name).map_err(|_| zferror!(ErrorKind::NotFound))?;

                Ok(lib)
            }
        },
        None => zenoh_flow::bail!(ErrorKind::NotFound, "Unable to find Python library, maybe need to install libpython3.x-dev or python3.x-dev"),
    }
}

fn read_file(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?)
}
