use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Color32F, Frame, Renderer,
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform},
};

use crate::shell::element::surface::BlurState;

use super::element::AsGlowRenderer;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Blurred<E> {
    elem: E,
    blur_state: BlurState,
}
impl<E> Blurred<E> {
    pub fn new(elem: E, blur_state: BlurState) -> Self {
        Self { elem, blur_state }
    }

    pub fn inner(&self) -> &E {
        &self.elem
    }

    pub fn inner_mut(&mut self) -> &mut E {
        &mut self.elem
    }
}

impl<E> Element for Blurred<E>
where
    E: Element,
{
    fn id(&self) -> &Id {
        self.elem.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.elem.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.elem.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.elem.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.elem.location(scale)
    }

    fn transform(&self) -> Transform {
        self.elem.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.elem.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.elem.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.elem.alpha()
    }

    fn kind(&self) -> Kind {
        self.elem.kind()
    }
}

pub trait BlurCapableRenderer {}

impl<R, E> RenderElement<R> for Blurred<E>
where
    R: Renderer + BlurCapableRenderer,
    E: RenderElement<R>,
{
    fn draw(
        &self,
        frame: &mut <R as smithay::backend::renderer::Renderer>::Frame<'_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), <R as smithay::backend::renderer::Renderer>::Error> {
        match self.blur_state {
            BlurState::Unblurred => {}
            BlurState::Blurred => {
                frame.draw_solid(dst, damage, Color32F::BLACK)?;
            }
            BlurState::PartiallyBlurred(_) => todo!(),
        }
        self.elem.draw(frame, src, dst, damage, opaque_regions)
    }
}

impl<T> BlurCapableRenderer for T where T: AsGlowRenderer {}
