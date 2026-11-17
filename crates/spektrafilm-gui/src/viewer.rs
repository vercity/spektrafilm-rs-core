use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use eframe::egui;
use spektrafilm_io::raster::RasterColor;
use spektrafilm_math::image::ImageBuf;

use crate::{
    PreviewTextureData, development_animation::Transition, make_preview_texture_data, paper_back,
    preview_source::PreviewSource,
};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(super) enum Layer {
    #[default]
    PaperBack,
    Input,
    Output,
}

#[derive(Clone)]
pub(super) struct InputLayerRequest {
    pub image: Arc<ImageBuf>,
    pub color: RasterColor,
    pub convert_raw: Option<RasterColor>,
    pub cap: u32,
}

impl PartialEq for InputLayerRequest {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.image, &other.image)
            && self.color == other.color
            && self.convert_raw == other.convert_raw
            && self.cap == other.cap
    }
}

const ANIMATION_INTERVAL: Duration = Duration::from_millis(32);

struct Animation {
    started: Instant,
    step: usize,
    transition: Transition,
    final_image: PreviewTextureData,
}

#[derive(Default)]
pub(super) struct Viewer {
    pub layer: Layer,
    paper: Option<([usize; 2], egui::TextureHandle)>,
    input: Option<(InputLayerRequest, egui::TextureHandle)>,
    input_job: Option<(InputLayerRequest, mpsc::Receiver<PreviewTextureData>)>,
    output_frame: Option<PreviewTextureData>,
    animation: Option<Animation>,
}

impl Viewer {
    pub fn source_changed(&mut self) {
        self.layer = Layer::PaperBack;
        self.input = None;
        self.clear_output();
    }

    pub fn clear_output(&mut self) {
        self.output_frame = None;
        self.animation = None;
    }

    pub fn show_output(
        &mut self,
        ctx: &egui::Context,
        texture: &mut Option<egui::TextureHandle>,
        target: PreviewTextureData,
        interpolation: &str,
    ) {
        let transition = Transition::prepare(
            self.output_frame.take(),
            &target,
            self.layer == Layer::Output && texture.is_some(),
        );
        self.layer = Layer::Output;
        if let Some(transition) = transition {
            self.present_output(
                ctx,
                texture,
                transition.frame(&target, 0).unwrap(),
                interpolation,
            );
            self.animation = Some(Animation {
                started: Instant::now(),
                step: 0,
                transition,
                final_image: target,
            });
            ctx.request_repaint_after(ANIMATION_INTERVAL);
        } else {
            self.replace_output(ctx, texture, target, interpolation);
        }
    }

    pub fn advance_output(
        &mut self,
        ctx: &egui::Context,
        texture: &mut Option<egui::TextureHandle>,
        interpolation: &str,
    ) {
        let Some(animation) = self.animation.as_mut() else {
            return;
        };
        let elapsed = animation.started.elapsed();
        let step = (elapsed.as_millis() / ANIMATION_INTERVAL.as_millis()) as usize;
        let visible = texture.is_some() && self.layer == Layer::Output;
        if visible && step == animation.step {
            ctx.request_repaint_after(ANIMATION_INTERVAL);
            return;
        }
        animation.step = step;
        let frame = visible
            .then(|| animation.transition.frame(&animation.final_image, step))
            .flatten();
        if let Some(frame) = frame {
            self.present_output(ctx, texture, frame, interpolation);
            ctx.request_repaint_after(ANIMATION_INTERVAL);
        } else {
            let animation = self.animation.take().unwrap();
            self.present_output(ctx, texture, animation.final_image, interpolation);
        }
    }

    pub fn replace_output(
        &mut self,
        ctx: &egui::Context,
        texture: &mut Option<egui::TextureHandle>,
        target: PreviewTextureData,
        interpolation: &str,
    ) {
        self.animation = None;
        self.present_output(ctx, texture, target, interpolation);
    }

    fn present_output(
        &mut self,
        ctx: &egui::Context,
        texture: &mut Option<egui::TextureHandle>,
        target: PreviewTextureData,
        interpolation: &str,
    ) {
        let sampler = if interpolation == "nearest" {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        };
        let display_image = egui::ColorImage::from_rgba_unmultiplied(target.size, &target.rgba);
        match texture {
            Some(texture) => texture.set(display_image, sampler),
            None => *texture = Some(ctx.load_texture("spektrafilm-output", display_image, sampler)),
        }
        self.output_frame = Transition::can_animate(target.size).then_some(target);
    }

    pub fn layers(&mut self, ui: &mut egui::Ui, input: bool, output: bool) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.layer, Layer::PaperBack, "Paper back");
            ui.add_enabled_ui(input, |ui| {
                ui.selectable_value(&mut self.layer, Layer::Input, "Input");
            });
            ui.add_enabled_ui(output, |ui| {
                ui.selectable_value(&mut self.layer, Layer::Output, "Output");
            });
        });
    }

    pub fn paper_texture(
        &mut self,
        ctx: &egui::Context,
        image: Option<&ImageBuf>,
    ) -> egui::TextureHandle {
        let [w, h] = image.map_or([2, 3], |image| [image.width, image.height]);
        let dimensions = paper_back::dimensions(w, h);
        if let Some((_, texture)) = self.paper.as_ref().filter(|(size, _)| *size == dimensions) {
            return texture.clone();
        }
        let pixels = paper_back::render(dimensions);
        let texture = layer_texture(ctx, "spektrafilm-paper-back", pixels);
        self.paper = Some((dimensions, texture.clone()));
        texture
    }

    pub fn input_texture(
        &mut self,
        ctx: &egui::Context,
        request: InputLayerRequest,
    ) -> Option<egui::TextureHandle> {
        if let Some((pending, reply)) = &self.input_job {
            match reply.try_recv() {
                Ok(data) => {
                    if pending == &request {
                        let texture = layer_texture(ctx, "spektrafilm-input", data);
                        self.input = Some((pending.clone(), texture));
                    }
                    self.input_job = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.input_job = None,
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(16))
                }
            }
        }
        if let Some((_, texture)) = self.input.as_ref().filter(|(key, _)| key == &request) {
            return Some(texture.clone());
        }
        if self.input_job.is_none() {
            let (tx, rx) = mpsc::channel();
            let work = request.clone();
            let ctx = ctx.clone();
            let worker = std::thread::Builder::new()
                .name("spektrafilm-source-preview".into())
                .spawn(move || {
                    let image =
                        PreviewSource::prepare(&mut None, work.image, work.cap, work.convert_raw);
                    let _ = tx.send(make_preview_texture_data(&image, Some(work.color)));
                    ctx.request_repaint();
                });
            match worker {
                Ok(_) => self.input_job = Some((request, rx)),
                Err(error) => tracing::warn!(%error, "could not prepare input layer"),
            }
        }
        None
    }
}

fn layer_texture(ctx: &egui::Context, name: &str, data: PreviewTextureData) -> egui::TextureHandle {
    ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied(data.size, &data.rgba),
        egui::TextureOptions::LINEAR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SRGB_DISPLAY;

    #[test]
    fn transitions_reuse_the_output_texture_and_settle_on_original_samples() {
        let ctx = egui::Context::default();
        let mut viewer = Viewer::default();
        let mut texture = None;
        let target = |rgb: [u8; 3]| PreviewTextureData {
            size: [1, 1],
            rgba: [rgb[0], rgb[1], rgb[2], 255].to_vec(),
        };
        viewer.show_output(&ctx, &mut texture, target([40, 50, 60]), "nearest");
        let texture_id = texture.as_ref().unwrap().id();
        assert!(matches!(
            viewer.animation.as_ref().unwrap().transition,
            Transition::Develop(_)
        ));
        assert_ne!(
            viewer.output_frame.as_ref().unwrap().rgba,
            [40, 50, 60, 255]
        );
        viewer.layer = Layer::Input;
        viewer.advance_output(&ctx, &mut texture, "nearest");
        assert!(viewer.animation.is_none());
        assert_eq!(
            viewer.output_frame.as_ref().unwrap().rgba,
            [40, 50, 60, 255]
        );
        assert_eq!(texture.as_ref().unwrap().id(), texture_id);

        viewer.layer = Layer::Output;
        viewer.show_output(&ctx, &mut texture, target([80, 10, 26]), "linear");
        assert!(matches!(
            viewer.animation.as_ref().unwrap().transition,
            Transition::Crossfade(_)
        ));
        assert_eq!(
            viewer.output_frame.as_ref().unwrap().rgba,
            [40, 50, 60, 255]
        );
        viewer.animation.as_mut().unwrap().started = Instant::now() - Duration::from_millis(500);
        viewer.advance_output(&ctx, &mut texture, "linear");
        assert!(viewer.animation.is_none());
        assert_eq!(
            viewer.output_frame.as_ref().unwrap().rgba,
            [80, 10, 26, 255]
        );
        assert_eq!(texture.as_ref().unwrap().id(), texture_id);
    }

    #[test]
    fn switching_sources_discards_an_in_flight_input_layer() {
        let mut viewer = Viewer::default();
        viewer.layer = Layer::Input;
        let ctx = egui::Context::default();
        let old = InputLayerRequest {
            image: Arc::new(ImageBuf::new(30, 20)),
            color: SRGB_DISPLAY,
            convert_raw: None,
            cap: 10,
        };
        assert!(viewer.input_texture(&ctx, old).is_none());
        let new = InputLayerRequest {
            image: Arc::new(ImageBuf::new(60, 30)),
            color: SRGB_DISPLAY,
            convert_raw: None,
            cap: 10,
        };
        viewer.source_changed();
        assert!(viewer.layer == Layer::PaperBack);
        let deadline = Instant::now() + Duration::from_secs(5);
        let texture = loop {
            if let Some(texture) = viewer.input_texture(&ctx, new.clone()) {
                break texture;
            }
            assert!(Instant::now() < deadline, "input texture worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(texture.size(), [10, 5]);
    }
}
