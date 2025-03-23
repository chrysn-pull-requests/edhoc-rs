use lakers::*;
use lakers_crypto::{default_crypto, CryptoTrait};
use log::trace;
use pyo3::{prelude::*, types::PyBytes};

use super::{ErrExt as _, StateMismatch};

#[pyclass(name = "EdhocResponder")]
pub struct PyEdhocResponder {
    r: Vec<u8>,
    cred_r: Credential,
    start: Option<ResponderStart>,
    processing_m1: Option<ProcessingM1>,
    wait_m3: Option<WaitM3>,
    processing_m3: Option<ProcessingM3>,
    processed_m3: Option<ProcessedM3>,
    completed: Option<Completed>,
}

/// Summary of a [`PyEdhocResponder`]'s state.
///
/// This is sorted along the typical (and, really, only) sequence of operations so that if the
/// expected state is greater than the current state, the user forgot to do something, whereas the
/// other way round, the user already did something and can't do that again.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum PyEdhocResponderSummary {
    Start,
    ProcessingM1,
    WaitM3,
    ProcessingM3,
    ProcessedM3,
    Completed,
    /// This is the last item because there is always something that happened before this and that
    /// broke things.
    Invalid,
}

#[pymethods]
impl PyEdhocResponder {
    #[new]
    fn new(r: Vec<u8>, py: Python<'_>, cred_r: super::AutoCredential) -> PyResult<Self> {
        trace!("Initializing EdhocResponder");
        let (y, g_y) = default_crypto().p256_generate_key_pair();

        let cred_r = cred_r
            .to_credential()
            .with_cause(py, "Failed to ingest CRED_R")?;

        Ok(Self {
            r,
            cred_r,
            start: Some(ResponderStart {
                method: EDHOCMethod::StatStat.into(),
                y,
                g_y,
            }),
            processing_m1: None,
            wait_m3: None,
            processing_m3: None,
            processed_m3: None,
            completed: None,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "<lakers.EdhocResponder at {:p} in state {:?}>",
            self,
            self.summarize()
        )
    }

    fn process_message_1<'a>(
        &mut self,
        py: Python<'a>,
        message_1: Vec<u8>,
    ) -> PyResult<(Bound<'a, PyBytes>, Option<EADItem>)> {
        let message_1 = EdhocMessageBuffer::new_from_slice(message_1.as_slice())
            .with_cause(py, "Message 1 too long")?;
        let (state, c_i, ead_1) =
            r_process_message_1(&self.take_start()?, &mut default_crypto(), &message_1)?;
        self.processing_m1 = Some(state);
        let c_i = PyBytes::new_bound(py, c_i.as_slice());

        Ok((c_i, ead_1))
    }

    #[pyo3(signature = (cred_transfer, c_r=None, ead_2=None))]
    fn prepare_message_2<'a>(
        &mut self,
        py: Python<'a>,
        cred_transfer: CredentialTransfer,
        c_r: Option<Vec<u8>>,
        ead_2: Option<EADItem>,
    ) -> PyResult<Bound<'a, PyBytes>> {
        let c_r = match c_r {
            Some(c_r) => ConnId::from_slice(c_r.as_slice())
                .with_cause(py, "Connection identifier C_R out of range")?,
            None => generate_connection_identifier_cbor(&mut default_crypto()),
        };
        let mut r = BytesP256ElemLen::default();
        r.copy_from_slice(self.r.as_slice());

        let (state, message_2) = r_prepare_message_2(
            self.as_ref_processing_m1()?,
            &mut default_crypto(),
            self.cred_r,
            &r,
            c_r,
            cred_transfer,
            &ead_2,
        )?;
        self.wait_m3 = Some(state);
        Ok(PyBytes::new_bound(py, message_2.as_slice()))
    }

    pub fn parse_message_3<'a>(
        &mut self,
        py: Python<'a>,
        message_3: Vec<u8>,
    ) -> PyResult<(Bound<'a, PyBytes>, Option<EADItem>)> {
        let message_3 = EdhocMessageBuffer::new_from_slice(message_3.as_slice())
            .with_cause(py, "Message 3 too long")?;
        let (state, id_cred_i, ead_3) =
            r_parse_message_3(&mut self.take_wait_m3()?, &mut default_crypto(), &message_3)?;
        self.processing_m3 = Some(state);
        Ok((PyBytes::new_bound(py, id_cred_i.bytes.as_slice()), ead_3))
    }

    pub fn verify_message_3<'a>(
        &mut self,
        py: Python<'a>,
        valid_cred_i: super::AutoCredential,
    ) -> PyResult<Bound<'a, PyBytes>> {
        let valid_cred_i = valid_cred_i
            .to_credential()
            .with_cause(py, "Failed to ingest CRED_I")?;
        let (state, prk_out) = r_verify_message_3(
            &mut self.take_processing_m3()?,
            &mut default_crypto(),
            valid_cred_i,
        )?;
        self.processed_m3 = Some(state);
        Ok(PyBytes::new_bound(py, prk_out.as_slice()))
    }

    #[pyo3(signature = (ead_4=None))]
    fn prepare_message_4<'a>(
        &mut self,
        py: Python<'a>,
        ead_4: Option<EADItem>,
    ) -> PyResult<Bound<'a, PyBytes>> {
        let (state, message_4) =
            r_prepare_message_4(&self.take_processed_m3()?, &mut default_crypto(), &ead_4)?;
        self.completed = Some(state);
        Ok(PyBytes::new_bound(py, message_4.as_slice()))
    }

    pub fn completed_without_message_4<'a>(&mut self, py: Python<'a>) -> PyResult<()> {
        let state = r_complete_without_message_4(&self.take_processed_m3()?)?;
        self.completed = Some(state);
        Ok(())
    }

    pub fn edhoc_exporter<'a>(
        &mut self,
        py: Python<'a>,
        label: u8,
        context: Vec<u8>,
        length: usize,
    ) -> PyResult<Bound<'a, PyBytes>> {
        let mut context_buf: BytesMaxContextBuffer = [0x00u8; MAX_KDF_CONTEXT_LEN];
        context_buf[..context.len()].copy_from_slice(context.as_slice());

        let res = edhoc_exporter(
            self.as_mut_completed()?,
            &mut default_crypto(),
            label,
            &context_buf,
            context.len(),
            length,
        );
        Ok(PyBytes::new_bound(py, &res[..length]))
    }

    pub fn edhoc_key_update<'a>(
        &mut self,
        py: Python<'a>,
        context: Vec<u8>,
    ) -> PyResult<Bound<'a, PyBytes>> {
        let mut context_buf = [0x00u8; MAX_KDF_CONTEXT_LEN];
        context_buf[..context.len()].copy_from_slice(context.as_slice());

        let res = edhoc_key_update(
            self.as_mut_completed()?,
            &mut default_crypto(),
            &context_buf,
            context.len(),
        );
        Ok(PyBytes::new_bound(py, &res[..SHA256_DIGEST_LEN]))
    }
}

/// Tools for generating useful and readable reprs and errors.
///
/// See [`StateMismatch`] for some more context.
impl PyEdhocResponder {
    fn summarize(&self) -> PyEdhocResponderSummary {
        let start = self.start.is_some();
        let processing_m1 = self.processing_m1.is_some();
        let wait_m3 = self.wait_m3.is_some();
        let processing_m3 = self.processing_m3.is_some();
        let processed_m3 = self.processed_m3.is_some();
        let completed = self.completed.is_some();
        match (
            start,
            processing_m1,
            wait_m3,
            processing_m3,
            processed_m3,
            completed,
        ) {
            (true, false, false, false, false, false) => PyEdhocResponderSummary::Start,
            (false, true, false, false, false, false) => PyEdhocResponderSummary::ProcessingM1,
            (false, false, true, false, false, false) => PyEdhocResponderSummary::WaitM3,
            (false, false, false, true, false, false) => PyEdhocResponderSummary::ProcessingM3,
            (false, false, false, false, true, false) => PyEdhocResponderSummary::ProcessedM3,
            (false, false, false, false, false, true) => PyEdhocResponderSummary::Completed,
            _ => PyEdhocResponderSummary::Invalid,
        }
    }

    // FIXME: Those should be generated, or the PyEdhocResponder type changed to something more
    // annotated-enum-ish

    fn take_start(&mut self) -> Result<ResponderStart, StateMismatch<PyEdhocResponderSummary>> {
        let summary = self.summarize();
        match self.start.take() {
            Some(o) => Ok(o),
            None => Err(StateMismatch::new(PyEdhocResponderSummary::Start, summary)),
        }
    }

    fn as_ref_processing_m1(
        &self,
    ) -> Result<&ProcessingM1, StateMismatch<PyEdhocResponderSummary>> {
        let summary = self.summarize();
        match self.processing_m1.as_ref() {
            Some(o) => Ok(o),
            None => Err(StateMismatch::new(
                PyEdhocResponderSummary::ProcessingM1,
                summary,
            )),
        }
    }

    fn take_wait_m3(&mut self) -> Result<WaitM3, StateMismatch<PyEdhocResponderSummary>> {
        let summary = self.summarize();
        match self.wait_m3.take() {
            Some(o) => Ok(o),
            None => Err(StateMismatch::new(PyEdhocResponderSummary::WaitM3, summary)),
        }
    }

    fn take_processing_m3(
        &mut self,
    ) -> Result<ProcessingM3, StateMismatch<PyEdhocResponderSummary>> {
        let summary = self.summarize();
        match self.processing_m3.take() {
            Some(o) => Ok(o),
            None => Err(StateMismatch::new(
                PyEdhocResponderSummary::ProcessingM3,
                summary,
            )),
        }
    }

    fn take_processed_m3(&mut self) -> Result<ProcessedM3, StateMismatch<PyEdhocResponderSummary>> {
        let summary = self.summarize();
        match self.processed_m3.take() {
            Some(o) => Ok(o),
            None => Err(StateMismatch::new(
                PyEdhocResponderSummary::ProcessedM3,
                summary,
            )),
        }
    }

    fn as_mut_completed(
        &mut self,
    ) -> Result<&mut Completed, StateMismatch<PyEdhocResponderSummary>> {
        let summary = self.summarize();
        match self.completed.as_mut() {
            Some(o) => Ok(o),
            None => Err(StateMismatch::new(
                PyEdhocResponderSummary::Completed,
                summary,
            )),
        }
    }
}
