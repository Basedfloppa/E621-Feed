use crate::TagCount;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use web_sys::{
    js_sys, CanvasRenderingContext2d, HtmlCanvasElement, MutationObserver, MutationObserverInit,
};
use yew::{
    classes, function_component, html, use_effect, use_effect_with, use_state, Callback, Html,
    NodeRef, Properties,
};
use yew::{use_memo, UseStateHandle};

#[derive(Properties, PartialEq)]
pub struct TagChartCardProps {
    pub canvas_ref: NodeRef,
    pub tag_counts: UseStateHandle<Vec<TagCount>>,
}

#[function_component(TagChartCard)]
pub fn tag_chart_card(props: &TagChartCardProps) -> Html {
    let theme_trigger = use_state(|| 0);
    let selected_group = use_state(String::new);
    let resize_trigger = use_state(|| 0);
    // Force re-initialization when tag_counts become non-empty after being empty.
    let _has_data_trigger = use_state(|| false);

    let current_tags = use_memo(
        (selected_group.clone(), props.tag_counts.clone()),
        |(group, tags)| {
            let mut filtered: Vec<TagCount> = tags
                .iter()
                .filter(|tag| tag.group_type == **group)
                .cloned()
                .collect();
            filtered.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
            filtered
        },
    );

    let group_types = use_memo(props.tag_counts.clone(), |tag_counts| {
        let mut groups: Vec<String> = tag_counts
            .iter()
            .map(|tag| tag.group_type.clone())
            .collect();
        groups.sort();
        groups.dedup();
        groups
    });

    use_effect_with(
        (
            selected_group.clone(),
            group_types.clone(),
            props.tag_counts.len(),
        ),
        |(selected_group, group_types, _count)| {
            if !selected_group.is_empty() && group_types.contains(&**selected_group) {
                return;
            }

            if let Some(first) = group_types.first() {
                selected_group.set(first.clone());
            }
        },
    );

    use_effect({
        let theme_trigger = theme_trigger.clone();
        move || {
            let document = web_sys::window().unwrap().document().unwrap();
            let target = document.body().unwrap();

            let trigger = theme_trigger.clone();
            let callback = Closure::<dyn FnMut(js_sys::Array, _)>::new(
                move |mutations: js_sys::Array, _obs: MutationObserver| {
                    for i in 0..mutations.length() {
                        let mutation = mutations.get(i).dyn_into::<web_sys::MutationRecord>().ok();
                        if let Some(m) = mutation {
                            let attr = m.attribute_name();
                            if attr.as_deref() == Some("data-theme")
                                || attr.as_deref() == Some("class")
                            {
                                trigger.set(*trigger + 1);
                                break;
                            }
                        }
                    }
                },
            );

            let observer = MutationObserver::new(callback.as_ref().unchecked_ref()).unwrap();
            let options = MutationObserverInit::new();
            options.set_attributes(true);
            observer.observe_with_options(&target, &options).unwrap();

            // Keep the callback alive for the lifetime of the observer.
            callback.forget();

            move || {
                observer.disconnect();
            }
        }
    });

    use_effect({
        let resize_trigger = resize_trigger.clone();
        move || {
            let closure = Closure::<dyn FnMut()>::new(move || {
                resize_trigger.set(*resize_trigger + 1);
            });

            let window = web_sys::window().unwrap();
            window
                .add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
                .unwrap();

            move || {
                window
                    .remove_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
                    .unwrap();
            }
        }
    });

    use_effect_with(
        (
            props.canvas_ref.clone(),
            current_tags.clone(),
            *theme_trigger,
            *resize_trigger,
        ),
        |(canvas_ref, current_tags, _, _)| {
            if let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() {
                draw_chart(&canvas, current_tags);
            }
            || ()
        },
    );

    let on_tab_click = {
        let selected_group = selected_group.clone();
        Callback::from(move |group: String| {
            selected_group.set(group);
        })
    };

    if props.tag_counts.is_empty() {
        return html! {};
    }

    html! {
        <div class="card bg-base-100 shadow mt-4">
            <div class="bg-primary text-primary-content p-4">
                <h5 class="card-title text-lg text-primary-content">{"Tag Analysis"}</h5>
            </div>
            <div class="card-body text-base-content">
                <div class="flex flex-wrap gap-2 mb-3">
                    {
                        for group_types.iter().map(|group| {
                            let is_active = *group == *selected_group;
                            let group_clone = group.clone();
                            html! {
                                <button
                                    class={classes!(
                                        "px-3", "py-1.5", "rounded-lg", "text-sm",
                                        "font-medium", "transition-all", "duration-150", "cursor-pointer",
                                        if is_active {
                                            "bg-primary text-primary-content shadow-sm"
                                        } else {
                                            "bg-base-200 text-base-content/70 hover:bg-base-300 hover:text-base-content hover:shadow-sm"
                                        }
                                    )}
                                    onclick={on_tab_click.reform(move |_| group_clone.clone())}
                                >
                                    { group }
                                </button>
                            }
                        })
                    }
                </div>

                <div class="chart-container" style="max-width: 100%;">
                    <canvas
                        ref={props.canvas_ref.clone()}
                        style="display: block; width: 100%"
                    />
                </div>
            </div>
        </div>
    }
}

fn draw_chart(canvas: &HtmlCanvasElement, tag_counts: &[TagCount]) {
    let window = web_sys::window().expect("no global window exists");
    let device_pixel_ratio = window.device_pixel_ratio();

    let logical_width = canvas.client_width() as f64;
    if logical_width == 0.0 || tag_counts.is_empty() {
        return;
    }

    let bar_spacing = 30.0;
    let top_padding = 30.0;
    let bottom_padding = 30.0;

    let ideal_height = top_padding + bottom_padding + (bar_spacing * tag_counts.len() as f64);
    let max_logical_height = 2048.0;
    let logical_height = ideal_height.min(max_logical_height);

    let physical_width = (logical_width * device_pixel_ratio).round();
    let physical_height = (logical_height * device_pixel_ratio).round();

    let prev_width = canvas.width();
    let prev_height = canvas.height();

    if prev_width != physical_width as u32 {
        canvas.set_width(physical_width as u32);
    }
    if prev_height != physical_height as u32 {
        canvas.set_height(physical_height as u32);
    }

    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into()
        .unwrap();

    if physical_height <= 32767.0 && physical_width <= 32767.0 {
        ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0).unwrap();
        ctx.scale(device_pixel_ratio, device_pixel_ratio)
            .expect("Failed to scale context");
    }

    ctx.clear_rect(0.0, 0.0, logical_width, logical_height);

    if tag_counts.is_empty() {
        return;
    }

    ctx.set_font("bold 12px Arial");
    let mut max_left_text_width: f64 = 0.0;
    let mut max_right_text_width: f64 = 0.0;

    for tag in tag_counts {
        let name_w: f64 = ctx.measure_text(&tag.name).unwrap().width();
        let count_w: f64 = ctx.measure_text(&tag.count.to_string()).unwrap().width();
        max_left_text_width = max_left_text_width.max(name_w);
        max_right_text_width = max_right_text_width.max(count_w);
    }

    let left_padding = max_left_text_width + 20.0;
    let right_padding = max_right_text_width + 40.0;

    let chart_width = logical_width - left_padding - right_padding;
    if chart_width <= 0.0 {
        return;
    }

    let chart_height = logical_height - top_padding - bottom_padding;
    let bar_height = bar_spacing.min(chart_height / tag_counts.len() as f64);
    let max_value = tag_counts.iter().map(|t| t.count).max().unwrap_or(1) as f64;

    let el: &web_sys::Element = canvas.as_ref();

    let colors = [
        get_css_variable_value_on(el, "--color-primary").unwrap_or("oklch(55% 0.3 240)".into()),
        get_css_variable_value_on(el, "--color-success").unwrap_or("oklch(65% 0.25 140)".into()),
        get_css_variable_value_on(el, "--color-info").unwrap_or("oklch(70% 0.2 220)".into()),
        get_css_variable_value_on(el, "--color-warning").unwrap_or("oklch(80% 0.25 80)".into()),
        get_css_variable_value_on(el, "--color-error").unwrap_or("oklch(65% 0.3 30)".into()),
        get_css_variable_value_on(el, "--color-secondary").unwrap_or("oklch(70% 0.25 200)".into()),
        get_css_variable_value_on(el, "--color-neutral").unwrap_or("oklch(50% 0.05 240)".into()),
    ];
    let text_color = get_css_variable_value_on(el, "--color-base-content")
        .unwrap_or("oklch(21% 0.006 285.885)".into());

    for (i, tag) in tag_counts.iter().enumerate() {
        let y = top_padding + i as f64 * bar_spacing;
        if y > logical_height - bottom_padding {
            break;
        }

        let bar_length = (tag.count as f64 / max_value) * chart_width;

        ctx.set_fill_style_str(&colors[i % colors.len()]);
        ctx.fill_rect(
            left_padding,
            y + (bar_height - 20.0) / 2.0,
            bar_length,
            20.0,
        );

        ctx.set_fill_style_str(&text_color);
        ctx.set_text_align("right");
        ctx.set_text_baseline("middle");
        ctx.fill_text(&tag.name, left_padding - 10.0, y + bar_height / 2.0)
            .unwrap_or(());

        ctx.set_text_align("left");
        ctx.fill_text(
            &tag.count.to_string(),
            left_padding + bar_length + 10.0,
            y + bar_height / 2.0,
        )
        .unwrap_or(());
    }

    ctx.set_font("bold 14px Arial");
    ctx.set_text_align("center");
    ctx.fill_text("Tags", left_padding - 20.0, 15.0)
        .unwrap_or(());
    ctx.fill_text("Count", logical_width - right_padding + 20.0, 15.0)
        .unwrap_or(());
}

fn get_css_variable_value_on(el: &web_sys::Element, var_name: &str) -> Option<String> {
    let window = web_sys::window()?;
    let computed = window.get_computed_style(el).ok()??;
    computed
        .get_property_value(var_name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
