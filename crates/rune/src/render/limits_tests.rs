//! What a compositor asks of a device's limits (`Compositor::limits`), and which devices have it.

use super::{Gpu, capped};
use crate::compose::no_limits;

/// A compositor that sizes its buffers from the adapter's largest.
fn big_buffers(adapter: &wgpu::Limits) -> wgpu::Limits {
    wgpu::Limits { max_buffer_size: adapter.max_buffer_size, ..no_limits(adapter) }
}

/// Without a compositor, or with one that asks for nothing, a device is asked for the limits
/// Rune asks for itself, on a strong adapter and on a weak one.
#[test]
fn asking_for_nothing_leaves_runes_own_limits() {
    let weak = wgpu::Limits { max_texture_dimension_2d: 1024, ..wgpu::Limits::downlevel_webgl2_defaults() };
    for adapter in [wgpu::Limits::unlimited(), wgpu::Limits::default(), wgpu::Limits::downlevel_defaults(), weak] {
        for own in [wgpu::Limits::default(), wgpu::Limits::downlevel_defaults()] {
            let own = own.using_resolution(adapter.clone()).using_alignment(adapter.clone());
            assert_eq!(own.clone().or_better_values_from(&capped(&no_limits, &adapter)), own);
        }
    }
}

/// What a compositor asks for is raised field by field, and never beyond what the adapter has.
#[test]
fn a_compositor_raises_the_fields_it_asks_for_up_to_the_adapters() {
    let adapter = wgpu::Limits { max_buffer_size: 1 << 32, max_texture_array_layers: 2048, ..wgpu::Limits::default() };
    let wants = |adapter: &wgpu::Limits| wgpu::Limits { max_texture_array_layers: u32::MAX, ..big_buffers(adapter) };
    let own = wgpu::Limits::default();
    let required = own.clone().or_better_values_from(&capped(&wants, &adapter));
    assert_eq!(required, wgpu::Limits { max_buffer_size: 1 << 32, max_texture_array_layers: 2048, ..own });
}

/// A device made for a compositor has the limits it asked of the adapter and Rune's own
/// otherwise; a device made without them doesn't serve that compositor.
#[test]
fn a_device_serves_a_compositor_only_with_the_limits_it_wants() {
    let Ok(plain) = Gpu::try_new(Gpu::instance(), None) else { return eprintln!("skipped: no GPU") };
    let (made_with, adapters) = (plain.device.limits(), plain.adapter.limits().max_buffer_size);
    if adapters <= made_with.max_buffer_size { return eprintln!("skipped: the adapter has no larger buffers to ask for") }
    assert!(plain.has_limits(&no_limits));
    assert!(!plain.has_limits(&big_buffers));
    let gpu = Gpu::try_with(Gpu::instance(), None, wgpu::Features::empty(), &big_buffers).expect("a device with the adapter's buffer size");
    assert_eq!(gpu.device.limits(), wgpu::Limits { max_buffer_size: adapters, ..made_with });
    assert!(gpu.has_limits(&big_buffers) && gpu.has_limits(&no_limits));
}
