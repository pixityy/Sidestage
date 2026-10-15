//! Glue between the backends and the renderer: `sidestage-platform` knows nothing about the DSP,
//! it only calls a [`Processor`] built by the [`ProcessorFactory`] made here.

use std::sync::Arc;

use sidestage_dsp::{SharedParams, Spatializer};
use sidestage_platform::{Processor, ProcessorFactory};

/// The binaural renderer as a backend [`Processor`].
struct SpatialProcessor(Spatializer);

impl Processor for SpatialProcessor {
    #[inline]
    fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
        self.0.process(input, output, sidechain_peak);
    }
}

/// A factory that builds a fresh [`Spatializer`] for each output stream the backend opens. All
/// of them read the same `params`, so UI changes reach whichever stream is current.
pub fn factory(params: Arc<SharedParams>) -> ProcessorFactory {
    Arc::new(move |sample_rate| {
        log::debug!("building renderer at {sample_rate} Hz");
        Box::new(SpatialProcessor(Spatializer::new(sample_rate, Arc::clone(&params))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidestage_dsp::Settings;

    #[test]
    fn factory_builds_working_processor() {
        let f = factory(SharedParams::new(Settings::default()));
        let mut p = f(48_000);
        let input = vec![0.25f32; 2 * 480];
        let mut output = vec![0.0f32; 2 * 480];
        p.process(&input, &mut output, 0.0);
        assert!(output.iter().all(|s| s.is_finite()));
    }
}
