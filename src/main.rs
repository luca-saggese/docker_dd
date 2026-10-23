use anyhow::{Context, Result};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Sense, Shape, Stroke, Vec2};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::Instant,
};

#[derive(Clone, Debug)]
struct Resource {
    kind: String,
    label: String,
    detail: String,
    bytes: Option<u64>,
    chart_bytes: u64,
    docker_ref: String,
    usage_block: Option<String>,
    image_layers: Vec<ImageLayer>,
}
#[derive(Clone, Debug)]
struct ImageLayer {
    label: String,
    bytes: u64,
}
#[derive(Deserialize)]
struct HistoryRow {
    #[serde(rename = "CreatedBy", default)]
    created_by: String,
    #[serde(rename = "Size", default)]
    size: String,
}
#[derive(Default)]
struct Inventory {
    resources: Vec<Resource>,
    total_bytes: u64,
    error: Option<String>,
    scanned_at: Option<Instant>,
}

#[derive(Deserialize)]
struct Container {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "State")]
    state: Option<ContainerState>,
    #[serde(rename = "SizeRw")]
    rw: Option<i64>,
    #[serde(rename = "Image", default)]
    image_id: String,
    #[serde(rename = "Config")]
    config: Option<ContainerConfig>,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<ContainerMount>,
}
#[derive(Deserialize)]
struct ContainerState {
    #[serde(rename = "Status", default)]
    status: String,
}
#[derive(Deserialize)]
struct ContainerMount {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Name", default)]
    name: String,
}
#[derive(Deserialize)]
struct ContainerConfig {
    #[serde(rename = "Image", default)]
    image: String,
}
#[derive(Deserialize)]
struct Image {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "RepoTags", default)]
    tags: Vec<String>,
}
#[derive(Deserialize)]
struct Volume {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Driver", default)]
    driver: String,
    #[serde(rename = "UsageData")]
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct Usage {
    #[serde(rename = "Size", default)]
    size: i64,
}

fn docker(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("Docker CLI non trovato")?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn ids(args: &[&str]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    Ok(docker(args)?
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert((*s).to_owned()))
        .map(str::to_owned)
        .collect())
}
fn inspect<T: for<'de> Deserialize<'de>>(ids: &[String]) -> Result<Vec<T>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut args = vec!["inspect".to_owned()];
    args.extend(ids.iter().cloned());
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("lettura dettagli Docker non riuscita")?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(serde_json::from_slice(&output.stdout).context("risposta Docker non valida")?)
}
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s == "<unknown>" {
        return None;
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .unwrap_or(s.len());
    let number = s[..split].replace(',', ".").parse::<f64>().ok()?;
    let unit = s[split..].trim().to_ascii_lowercase();
    let factor = match unit.as_str() {
        "b" | "bytes" => 1.0,
        "kb" => 1_000.0,
        "kib" => 1_024.0,
        "mb" => 1_000_000.0,
        "mib" => 1_048_576.0,
        "gb" => 1_000_000_000.0,
        "gib" => 1_073_741_824.0,
        "tb" => 1_000_000_000_000.0,
        "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    Some((number * factor).round().max(0.0) as u64)
}
fn df_rows(value: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}
fn field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}
fn image_layers(image_id: &str) -> Vec<ImageLayer> {
    let Ok(output) = docker(&[
        "image",
        "history",
        "--no-trunc",
        "--format",
        "{{json .}}",
        image_id,
    ]) else {
        return Vec::new();
    };
    let mut layers: Vec<_> = output
        .lines()
        .filter_map(|line| serde_json::from_str::<HistoryRow>(line).ok())
        .filter_map(|row| {
            let bytes = parse_size(&row.size)?;
            (bytes > 0).then(|| ImageLayer {
                label: if row.created_by.trim().is_empty() {
                    "Layer Docker".into()
                } else {
                    row.created_by.trim().to_owned()
                },
                bytes,
            })
        })
        .collect();
    layers.reverse(); // Docker History è dal layer più recente al più vecchio.
    layers
}
fn scan() -> Inventory {
    let mut inv = Inventory::default();
    if let Err(e) = docker(&["info", "--format", "{{.ServerVersion}}"]) {
        inv.error = Some(format!("Docker non disponibile: {e:#}"));
        return inv;
    }
    let scanned = (|| -> Result<()> {
        let df: serde_json::Value =
            serde_json::from_str(&docker(&["system", "df", "-v", "--format", "{{json .}}"])?)
                .context("dettaglio spazio Docker non valido")?;
        let image_ids = ids(&["image", "ls", "-aq"])?;
        let container_ids = ids(&["container", "ls", "-aq"])?;
        let volume_ids = ids(&["volume", "ls", "-q"])?;
        let mut image_space = HashMap::new();
        for row in df_rows(&df, "Images") {
            image_space.insert(
                field(&row, "ID").to_owned(),
                parse_size(field(&row, "UniqueSize")).unwrap_or(0),
            );
        }
        let mut volume_space = HashMap::new();
        for row in df_rows(&df, "Volumes") {
            volume_space.insert(
                field(&row, "Name").trim_start_matches('/').to_owned(),
                parse_size(field(&row, "Size")),
            );
        }
        let mut container_space = HashMap::new();
        for row in df_rows(&df, "Containers") {
            container_space.insert(
                field(&row, "Names").to_owned(),
                parse_size(field(&row, "Size")),
            );
        }
        let containers = inspect::<Container>(&container_ids)?;
        let images = inspect::<Image>(&image_ids)?;
        let volumes = inspect::<Volume>(&volume_ids)?;
        for volume in &volumes {
            let name = volume.name.trim_start_matches('/');
            if !volume_space.contains_key(name) {
                volume_space.insert(
                    name.to_owned(),
                    volume
                        .usage
                        .as_ref()
                        .filter(|u| u.size >= 0)
                        .map(|u| u.size as u64),
                );
            }
        }
        let mut volume_owners: HashMap<String, Vec<String>> = HashMap::new();
        for (i, c) in containers.iter().enumerate() {
            let name = c
                .names
                .first()
                .map(|n| n.trim_start_matches('/'))
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    let n = c.name.trim_start_matches('/');
                    if n.is_empty() {
                        None
                    } else {
                        Some(n.to_owned())
                    }
                })
                .unwrap_or_else(|| format!("Container {}", i + 1));
            let mut seen = HashSet::new();
            for mount in c
                .mounts
                .iter()
                .filter(|m| m.kind == "volume" && !m.name.is_empty())
            {
                let volume = mount.name.trim_start_matches('/').to_owned();
                if seen.insert(volume.clone()) {
                    volume_owners.entry(volume).or_default().push(name.clone());
                }
            }
        }
        for (i, c) in containers.iter().enumerate() {
            let name = c
                .names
                .first()
                .map(|n| n.trim_start_matches('/'))
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    let n = c.name.trim_start_matches('/');
                    if n.is_empty() {
                        None
                    } else {
                        Some(n.to_owned())
                    }
                })
                .unwrap_or_else(|| format!("Container {}", i + 1));
            let image = c
                .config
                .as_ref()
                .map(|x| x.image.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("immagine non disponibile");
            let state = c
                .state
                .as_ref()
                .map(|s| s.status.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("stato sconosciuto");
            let writable = container_space
                .get(&name)
                .copied()
                .flatten()
                .or_else(|| c.rw.map(|n| n.max(0) as u64))
                .unwrap_or(0);
            let mut attached = HashSet::new();
            let volume_bytes = c
                .mounts
                .iter()
                .filter(|m| m.kind == "volume" && !m.name.is_empty())
                .filter_map(|m| {
                    let n = m.name.trim_start_matches('/');
                    if attached.insert(n.to_owned()) {
                        volume_space.get(n).copied().flatten()
                    } else {
                        None
                    }
                })
                .sum::<u64>();
            let mounts = attached.len();
            let is_running = state.eq_ignore_ascii_case("running");
            inv.resources.push(Resource {
                kind: "Container".into(),
                label: name.clone(),
                detail: format!("{state} · {image} · {mounts} volumi collegati"),
                bytes: Some(writable.saturating_add(volume_bytes)),
                chart_bytes: writable,
                docker_ref: c.id.clone(),
                usage_block: if is_running {
                    Some("Container in esecuzione: arrestalo prima di eliminarlo.".into())
                } else {
                    None
                },
                image_layers: Vec::new(),
            });
        }
        for (i, image) in images.iter().enumerate() {
            let id = &image.id;
            let dangling = image.tags.iter().all(|t| t == "<none>:<none>");
            let label = image
                .tags
                .iter()
                .find(|t| t.as_str() != "<none>:<none>")
                .cloned()
                .unwrap_or_else(|| format!("Immagine senza tag {}", i + 1));
            let referenced = containers.iter().any(|c| c.image_id == *id);
            inv.resources.push(Resource {
                kind: "Immagine".into(),
                label,
                detail: format!(
                    "{} · spazio esclusivo dei layer",
                    if dangling {
                        "Immagine senza tag"
                    } else {
                        "Immagine"
                    }
                ),
                bytes: Some(*image_space.get(id).unwrap_or(&0)),
                chart_bytes: *image_space.get(id).unwrap_or(&0),
                docker_ref: id.clone(),
                usage_block: if referenced {
                    Some("Immagine usata da uno o più container, anche arrestati.".into())
                } else {
                    None
                },
                image_layers: image_layers(id),
            });
        }
        for (i, volume) in volumes.iter().enumerate() {
            let driver = if volume.driver.is_empty() {
                "local"
            } else {
                &volume.driver
            };
            let clean_name = volume.name.trim_start_matches('/');
            let anonymous =
                clean_name.len() >= 32 && clean_name.chars().all(|c| c.is_ascii_hexdigit());
            let label = if anonymous {
                format!("Volume {}", i + 1)
            } else {
                clean_name.to_owned()
            };
            let size = volume_space.get(clean_name).copied().flatten();
            let owners = volume_owners
                .get(clean_name)
                .map(|v| v.join(", "))
                .unwrap_or_else(|| "nessun container".into());
            let usage_block = volume_owners
                .get(clean_name)
                .filter(|v| !v.is_empty())
                .map(|_| "Volume collegato a uno o più container, anche arrestati.".into());
            inv.resources.push(Resource {
                kind: "Volume".into(),
                label,
                detail: format!(
                    "{} · {owners} · driver {driver}",
                    if anonymous {
                        "Volume anonimo"
                    } else {
                        "Volume nominato"
                    }
                ),
                bytes: size,
                chart_bytes: size.unwrap_or(0),
                docker_ref: clean_name.to_owned(),
                usage_block,
                image_layers: Vec::new(),
            });
        }
        inv.total_bytes = inv.resources.iter().map(|r| r.chart_bytes).sum();
        Ok(())
    })();
    if let Err(e) = scanned {
        inv.resources.clear();
        inv.error = Some(format!("Scansione incompleta: {e:#}"));
    }
    inv.scanned_at = Some(Instant::now());
    inv
}
fn fmt_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
fn fmt_opt_size(n: Option<u64>) -> String {
    n.map(fmt_size).unwrap_or_else(|| "n/d".into())
}

#[derive(Clone, Copy, PartialEq)]
enum ChartHit {
    Total,
    Group(&'static str),
    Resource(usize),
    ImageLayer(usize, usize),
}
struct App {
    data: Inventory,
    receiver: Option<Receiver<Inventory>>,
    busy: bool,
    selected: Option<usize>,
    animation_started: Instant,
    pending_delete: Option<usize>,
    delete_receiver: Option<Receiver<Result<(), String>>>,
    deleting: bool,
    delete_message: Option<String>,
    context_popup_open: bool,
    context_menu_target: Option<usize>,
}
impl App {
    fn new() -> Self {
        let mut a = Self {
            data: Inventory::default(),
            receiver: None,
            busy: false,
            selected: None,
            animation_started: Instant::now(),
            pending_delete: None,
            delete_receiver: None,
            deleting: false,
            delete_message: None,
            context_popup_open: false,
            context_menu_target: None,
        };
        a.refresh();
        a
    }
    fn refresh(&mut self) {
        if self.busy || self.deleting {
            return;
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(scan());
        });
        self.receiver = Some(rx);
        self.busy = true;
    }
    fn delete(&mut self, index: usize) {
        if self.deleting {
            return;
        }
        let Some(resource) = self.data.resources.get(index).cloned() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(delete_resource(&resource));
        });
        self.delete_receiver = Some(rx);
        self.deleting = true;
        self.pending_delete = None;
        self.delete_message = None;
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(rx) = &self.receiver {
            if let Ok(data) = rx.try_recv() {
                self.data = data;
                self.busy = false;
                self.receiver = None;
                self.selected = None;
                self.animation_started = Instant::now();
            }
        }
        if let Some(rx) = &self.delete_receiver {
            if let Ok(result) = rx.try_recv() {
                self.deleting = false;
                self.delete_receiver = None;
                self.delete_message = Some(match result {
                    Ok(()) => "Risorsa eliminata con Docker. Scansione aggiornata.".into(),
                    Err(e) => e,
                });
                if self.delete_message.as_deref()
                    == Some("Risorsa eliminata con Docker. Scansione aggiornata.")
                {
                    self.refresh();
                }
            }
        }
        let animation = (self.animation_started.elapsed().as_secs_f32() / 0.72).clamp(0.0, 1.0);
        let animation = 1.0 - (1.0 - animation).powi(3);
        ctx.request_repaint_after(std::time::Duration::from_millis(
            if animation < 1.0 || self.busy || self.deleting {
                16
            } else {
                200
            },
        ));
        let bg = Color32::from_rgb(37, 39, 44);
        let suppress_tooltips = self.context_popup_open
            || self.pending_delete.is_some()
            || ctx.input(|i| i.pointer.secondary_down() || i.pointer.secondary_clicked());
        self.context_popup_open = false;
        ctx.set_visuals(egui::Visuals::dark());
        let total = self.data.total_bytes;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(bg).inner_margin(egui::Margin::same(20))).show(ctx,|ui|{
        ui.horizontal(|ui|{ui.label(egui::RichText::new("Dischi e cartelle").color(Color32::from_rgb(112,167,255)).strong());ui.label(egui::RichText::new("›").color(Color32::GRAY));ui.label(egui::RichText::new("docker").strong());ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui|{if ui.add_enabled(!self.busy&&!self.deleting,egui::Button::new("⟳  Scansiona")).clicked(){self.refresh();}if self.busy{ui.spinner();ui.label("Scansione…");}if self.deleting{ui.spinner();ui.label("Verifica stato e rimozione Docker…");}});});
        if let Some(message)=&self.delete_message{ui.colored_label(if message.starts_with("Risorsa eliminata"){Color32::LIGHT_GREEN}else{Color32::LIGHT_RED},message);}
        ui.add_space(22.);
        let gap=22.;let available=ui.available_size();let left_width=(available.x-gap)*0.62;let right_width=(available.x-gap)-left_width;
        ui.horizontal(|ui|{
            ui.allocate_ui_with_layout(Vec2::new(left_width,available.y),egui::Layout::top_down(egui::Align::Min),|left|{
                let chart_height=(left.available_height()-38.).max(200.);
                let (rect,response)=left.allocate_exact_size(Vec2::new(left.available_width(),chart_height),Sense::click());
                let center=rect.center()-Vec2::new(0.,8.);let radius=rect.width().min(rect.height())*0.37;
                let hovered=response.hover_pos().and_then(|pos|hit_chart(pos,center,radius,&self.data.resources,total));
                draw_sunburst(left,center,radius,&self.data.resources,total,self.selected,hovered,animation);
                if response.clicked(){if let Some(pos)=response.interact_pointer_pos(){self.selected=match hit_chart(pos,center,radius,&self.data.resources,total){Some(ChartHit::Resource(i))=>Some(i),Some(ChartHit::ImageLayer(i,_))=>Some(i),_=>None};}}
                let chart_right_clicked = response.hovered() && ctx.input(|i| i.pointer.secondary_clicked());
                if chart_right_clicked {
                    self.context_menu_target = hovered.and_then(|hit| match hit {
                        ChartHit::Resource(i) | ChartHit::ImageLayer(i, _) => Some(i),
                        _ => None,
                    });
                }
                if !suppress_tooltips {
                if let(Some(hit),Some(pos))=(hovered,response.hover_pos()) {
                    let (title,detail,size)=match hit{
                        ChartHit::Total=>("Spazio totale Docker".to_owned(),"Layer univoci, layer scrivibili e volumi".to_owned(),total),
                        ChartHit::Group(group)=>{let title=if group=="Immagine"{"Immagini"}else{"Container e volumi"};let detail=if group=="Immagine"{"Dimensione esclusiva dei layer immagine"}else{"Layer scrivibili dei container e volumi"};(title.to_owned(),detail.to_owned(),group_total(&self.data.resources,group))},
                        ChartHit::Resource(i)=>{let r=&self.data.resources[i];let detail=if r.kind=="Immagine"&&!r.image_layers.is_empty(){format!("{} · {} layer visibili nell’anello esterno",r.detail,r.image_layers.len())}else{r.detail.clone()};(r.label.clone(),detail,r.bytes.unwrap_or(0))},
                        ChartHit::ImageLayer(i,j)=>{let layer=&self.data.resources[i].image_layers[j];(layer.label.clone(),"Layer riportato da docker image history (la dimensione può includere layer condivisi)".to_owned(),layer.bytes)},
                    };
                    egui::Area::new(egui::Id::new("docker-resource-tooltip")).order(egui::Order::Tooltip).fixed_pos(pos+Vec2::new(14.,14.)).interactable(false).show(ctx,|ui|{
                        egui::Frame::popup(ui.style()).show(ui,|ui|{ui.set_min_width(280.);ui.set_max_width(420.);ui.vertical(|ui|{ui.label(egui::RichText::new(title).strong());ui.label(detail);ui.label(fmt_size(size));});});
                    });
                }
                }
                let menu_target = self.context_menu_target.or_else(|| hovered.and_then(|hit| match hit {
                    ChartHit::Resource(i) | ChartHit::ImageLayer(i, _) => Some(i),
                    _ => None,
                }));
                let mut delete_request=None;response.context_menu(|ui|{match menu_target{Some(i)=>{let label=self.data.resources.get(i).map(|r|r.label.as_str()).unwrap_or("risorsa");if ui.button(format!("Elimina {label}…")).clicked(){delete_request=Some(i);ui.close();}},None=>{ui.label("Fai clic destro su una risorsa del grafico per eliminarla.");}}});if let Some(i)=delete_request{self.pending_delete=Some(i);}
                let chart_menu_open = response.context_menu_opened();
                self.context_popup_open |= chart_menu_open;
                if !chart_menu_open && !chart_right_clicked { self.context_menu_target = None; }
                let painter=left.painter();painter.circle_filled(center,radius*0.22,bg);painter.text(center-Vec2::new(0.,12.),Align2::CENTER_CENTER,fmt_size(total),FontId::proportional(22.),Color32::WHITE);painter.text(center+Vec2::new(0.,19.),Align2::CENTER_CENTER,"docker",FontId::proportional(14.),Color32::LIGHT_GRAY);
                left.with_layout(egui::Layout::bottom_up(egui::Align::Min),|ui|{ui.horizontal(|ui|{ui.label(egui::RichText::new("◉").size(27.).color(Color32::from_rgb(67,120,165)));ui.label(egui::RichText::new("Spazio Docker analizzato").color(Color32::from_rgb(145,158,175)));});});
            });
            ui.add_space(gap);
            ui.allocate_ui_with_layout(Vec2::new(right_width,available.y),egui::Layout::top_down(egui::Align::Min),|right|{
                right.horizontal(|ui|{ui.label(egui::RichText::new("docker").size(24.));ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui|{ui.label(egui::RichText::new(fmt_size(total)).size(23.));});});right.add_space(10.);
                right.label(egui::RichText::new(format!("{} risorse Docker",self.data.resources.len())).color(Color32::GRAY));right.add_space(8.);right.separator();
                if let Some(error)=&self.data.error{right.add(egui::Label::new(egui::RichText::new(error).color(Color32::LIGHT_RED)).wrap());right.add_space(8.);right.label("Avvia Docker Desktop o il demone Docker, poi ripeti la scansione.");}
                if self.data.resources.is_empty()&&self.data.error.is_none(){right.label("Nessuna risorsa Docker trovata.");}
                egui::ScrollArea::vertical().show(right,|right|{
                    let mut sections=vec![("Container",resource_color("Container",0),0u64),("Immagine",resource_color("Immagine",0),0u64),("Volume",resource_color("Volume",0),0u64)];
                    for(kind,_,sum)in &mut sections{*sum=self.data.resources.iter().filter(|r|r.kind==*kind).filter_map(|r|r.bytes).sum();}
                    sections.sort_by(|a,b|b.2.cmp(&a.2));
                    for(kind,color,sum)in sections{
                        let mut matches:Vec<(usize,&Resource)>=self.data.resources.iter().enumerate().filter(|(_,r)|r.kind==kind).collect();matches.sort_by(|a,b|b.1.bytes.unwrap_or(0).cmp(&a.1.bytes.unwrap_or(0)));
                        right.add_space(10.);right.horizontal(|ui|{ui.colored_label(color,"●");ui.label(egui::RichText::new(if kind=="Immagine"{"Immagini"}else if kind=="Container"{"Container"}else{"Volumi"}).size(18.));ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui|{ui.label(fmt_size(sum));});});
                        if matches.is_empty(){right.label(egui::RichText::new(if kind=="Immagine"{"Nessuna immagine"}else if kind=="Container"{"Nessun container"}else{"Nessun volume"}).color(Color32::GRAY));}
                        for(i,r)in matches{
                            let selected=self.selected==Some(i);let dot=resource_color(kind,i);
                            let row=right.horizontal(|ui|{ui.add_space(22.);ui.colored_label(dot,if selected{"●"}else{"◦"});ui.label(&r.label);ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui|{ui.label(fmt_opt_size(r.bytes));});}).response;
                            let response=right.interact(row.rect,egui::Id::new(("docker-resource-row",i)),Sense::click());
                            if response.clicked(){self.selected=Some(i);}
                            let mut delete_request=false;response.context_menu(|ui|{if ui.button(format!("Elimina {}…",r.label)).clicked(){delete_request=true;ui.close();}});
                            self.context_popup_open |= response.context_menu_opened();
                            if delete_request{self.pending_delete=Some(i);}if selected{right.label(egui::RichText::new(&r.detail).size(12.).color(Color32::GRAY));}
                        }
                    }
                });
                if let Some(r)=self.selected.and_then(|i|self.data.resources.get(i)){right.add_space(5.);right.separator();right.label(egui::RichText::new(format!("{}  ·  {}",r.kind,r.label)).strong());right.label(format!("{}    {}",r.detail,fmt_opt_size(r.bytes)));}
            });
        });
    });
        if let Some(i) = self.pending_delete {
            if let Some(resource) = self.data.resources.get(i).cloned() {
                let mut open = true;
                egui::Window::new("Conferma eliminazione").collapsible(false).resizable(false).open(&mut open).show(ctx,|ui|{ui.label(egui::RichText::new(format!("{} · {}",resource.kind,resource.label)).strong());ui.add_space(6.);ui.label(format!("Dimensione visualizzata: {}",fmt_opt_size(resource.bytes)));ui.label(&resource.detail);ui.add_space(8.);ui.label(match resource.kind.as_str(){"Container"=>"Docker eseguirà: docker container rm. Il comando non rimuove i volumi collegati.","Immagine"=>"Docker eseguirà: docker image rm. Se è in uso, Docker impedirà la rimozione.",_=>"Docker eseguirà: docker volume rm. Se è collegato a un container, Docker impedirà la rimozione."});if let Some(reason)=&resource.usage_block{ui.colored_label(Color32::YELLOW,reason);ui.label("Lo stato verrà ricontrollato prima di agire.");}ui.add_space(10.);ui.horizontal(|ui|{if ui.button("Annulla").clicked(){self.pending_delete=None;}if ui.add_enabled(!self.deleting,egui::Button::new("Conferma ed elimina con Docker")).clicked(){self.delete(i);}});});
                if !open {
                    self.pending_delete = None;
                }
            }
        }
    }
}
fn resource_color(kind: &str, index: usize) -> Color32 {
    let palette: &[Color32] = match kind {
        "Container" => &[
            Color32::from_rgb(144, 198, 164),
            Color32::from_rgb(127, 187, 182),
            Color32::from_rgb(181, 202, 145),
            Color32::from_rgb(113, 171, 142),
            Color32::from_rgb(151, 192, 181),
            Color32::from_rgb(194, 205, 150),
        ],
        "Immagine" => &[
            Color32::from_rgb(145, 166, 210),
            Color32::from_rgb(125, 188, 198),
            Color32::from_rgb(177, 157, 204),
            Color32::from_rgb(220, 198, 135),
            Color32::from_rgb(210, 157, 135),
            Color32::from_rgb(202, 144, 151),
            Color32::from_rgb(157, 192, 145),
            Color32::from_rgb(201, 151, 188),
            Color32::from_rgb(142, 157, 203),
        ],
        _ => &[
            Color32::from_rgb(211, 151, 163),
            Color32::from_rgb(218, 177, 139),
            Color32::from_rgb(195, 157, 190),
            Color32::from_rgb(208, 151, 143),
            Color32::from_rgb(178, 157, 198),
            Color32::from_rgb(211, 195, 143),
        ],
    };
    palette[index % palette.len()]
}
fn chart_items<'a>(items: &'a [Resource], group: &str) -> Vec<(usize, &'a Resource)> {
    let mut out: Vec<_> = items
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            if group == "Container" {
                r.kind == "Container" || r.kind == "Volume"
            } else {
                r.kind == group
            }
        })
        .collect();
    out.sort_by(|a, b| b.1.chart_bytes.cmp(&a.1.chart_bytes));
    out
}
fn group_total(items: &[Resource], group: &str) -> u64 {
    chart_items(items, group)
        .iter()
        .map(|(_, r)| r.chart_bytes)
        .sum()
}
fn wedge(
    p: &egui::Painter,
    center: Pos2,
    inner: f32,
    outer: f32,
    start: f32,
    sweep: f32,
    color: Color32,
    stroke: f32,
) {
    if sweep <= 0.0 || outer <= inner {
        return;
    }
    let steps = ((sweep * 90.0).ceil() as usize).clamp(1, 240);
    let mut outer_arc = Vec::with_capacity(steps + 1);
    let mut inner_arc = Vec::with_capacity(steps + 1);
    for s in 0..=steps {
        let a = start + sweep * s as f32 / steps as f32;
        let dir = Vec2::new(a.cos(), a.sin());
        outer_arc.push(center + dir * outer);
        inner_arc.push(center + dir * inner);
    }
    // Each thin quad is convex; filling the whole annular wedge as a single
    // convex polygon incorrectly fills the inner cutout for wide sectors.
    let mut mesh = egui::Mesh::default();
    mesh.reserve_vertices(steps * 4);
    mesh.reserve_triangles(steps * 2);
    for s in 0..steps {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(outer_arc[s], color);
        mesh.colored_vertex(outer_arc[s + 1], color);
        mesh.colored_vertex(inner_arc[s + 1], color);
        mesh.colored_vertex(inner_arc[s], color);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    p.add(Shape::mesh(mesh));
    if stroke > 0.0 {
        let edge = Stroke::new(stroke, Color32::from_rgb(40, 42, 47));
        let cuts = [
            [inner_arc[0], outer_arc[0]],
            [inner_arc[steps], outer_arc[steps]],
        ];
        p.add(Shape::line(outer_arc, edge));
        p.add(Shape::line(inner_arc, edge));
        for cut in cuts {
            p.line_segment(cut, edge);
        }
    }
}
fn draw_sunburst(
    ui: &egui::Ui,
    center: Pos2,
    radius: f32,
    items: &[Resource],
    total: u64,
    selected: Option<usize>,
    hovered: Option<ChartHit>,
    intro: f32,
) {
    let p = ui.painter();
    if total == 0 || items.is_empty() {
        p.circle_stroke(
            center,
            radius * 0.72,
            Stroke::new(1.0_f32, Color32::from_gray(80)),
        );
        return;
    }
    let root = Color32::from_rgb(105, 115, 130);
    wedge(
        p,
        center,
        radius * 0.22 * intro,
        radius * 0.40 * intro,
        -std::f32::consts::FRAC_PI_2,
        std::f32::consts::TAU,
        if hovered == Some(ChartHit::Total) {
            root.gamma_multiply(1.25)
        } else {
            root
        },
        0.0,
    );
    let mut angle = 0.0;
    for group in ["Immagine", "Container"] {
        let subtotal = group_total(items, group);
        if subtotal == 0 {
            continue;
        }
        let span = subtotal as f32 / total as f32 * std::f32::consts::TAU;
        let category = resource_color(group, 0);
        let group_hit = if group == "Immagine" {
            ChartHit::Group("Immagine")
        } else {
            ChartHit::Group("Container")
        };
        wedge(
            p,
            center,
            radius * 0.43 * intro,
            radius * 0.61 * intro,
            angle - std::f32::consts::FRAC_PI_2,
            span,
            category.gamma_multiply(if hovered == Some(group_hit) {
                1.14
            } else {
                0.93
            }),
            1.1,
        );
        let mut child = angle;
        for (i, item) in chart_items(items, group) {
            let bytes = item.chart_bytes;
            if bytes == 0 {
                continue;
            }
            let part = span * bytes as f32 / subtotal as f32;
            let color = resource_color(&item.kind, i);
            let is_hovered = hovered == Some(ChartHit::Resource(i));
            let hover = ui.ctx().animate_value_with_time(
                egui::Id::new(("resource-hover", i)),
                if is_hovered { 1.0 } else { 0.0 },
                0.16,
            );
            let fill = if selected == Some(i) {
                color.gamma_multiply(1.08)
            } else {
                color.gamma_multiply(0.95 + hover * 0.08)
            };
            let pop = hover * radius * 0.035;
            wedge(
                p,
                center,
                radius * 0.64 * intro,
                radius * 0.99 * intro + pop,
                child - std::f32::consts::FRAC_PI_2,
                part,
                fill,
                if selected == Some(i) || is_hovered {
                    2.0
                } else {
                    0.9
                },
            );
            if item.kind == "Immagine" && !item.image_layers.is_empty() {
                let layer_total: u64 = item.image_layers.iter().map(|layer| layer.bytes).sum();
                let mut layer_angle = child;
                for (j, layer) in item.image_layers.iter().enumerate() {
                    let layer_span = part * layer.bytes as f32 / layer_total.max(1) as f32;
                    let is_layer_hovered = hovered == Some(ChartHit::ImageLayer(i, j));
                    let layer_color = resource_color("Immagine", i + j + 1);
                    wedge(
                        p,
                        center,
                        radius * 1.025 * intro,
                        radius * 1.20 * intro,
                        layer_angle - std::f32::consts::FRAC_PI_2,
                        layer_span,
                        layer_color.gamma_multiply(if is_layer_hovered { 1.12 } else { 0.96 }),
                        if is_layer_hovered { 2.0 } else { 0.9 },
                    );
                    layer_angle += layer_span;
                }
            }
            child += part;
        }
        angle += span;
    }
}
fn hit_chart(p: Pos2, c: Pos2, r: f32, items: &[Resource], total: u64) -> Option<ChartHit> {
    let d = p - c;
    let radius = d.length();
    if total == 0 || radius > r * 1.20 {
        return None;
    }
    if radius >= r * 0.22 && radius <= r * 0.40 {
        return Some(ChartHit::Total);
    }
    let a = (d.y.atan2(d.x) + std::f32::consts::FRAC_PI_2).rem_euclid(std::f32::consts::TAU);
    let mut angle = 0.0;
    for group in ["Immagine", "Container"] {
        let subtotal = group_total(items, group);
        if subtotal == 0 {
            continue;
        }
        let span = subtotal as f32 / total as f32 * std::f32::consts::TAU;
        if radius >= r * 0.43 && radius <= r * 0.61 && a >= angle && a < angle + span {
            return Some(ChartHit::Group(if group == "Immagine" {
                "Immagine"
            } else {
                "Container"
            }));
        }
        if radius >= r * 1.02 && radius <= r * 1.20 {
            if group == "Immagine" {
                let mut child = angle;
                for (i, item) in chart_items(items, group) {
                    let item_bytes = item.chart_bytes;
                    if item_bytes == 0 {
                        continue;
                    }
                    let item_span = span * item_bytes as f32 / subtotal as f32;
                    if item.kind == "Immagine" && !item.image_layers.is_empty() {
                        let layer_total: u64 =
                            item.image_layers.iter().map(|layer| layer.bytes).sum();
                        let mut layer_angle = child;
                        for (j, layer) in item.image_layers.iter().enumerate() {
                            let layer_span =
                                item_span * layer.bytes as f32 / layer_total.max(1) as f32;
                            if a >= layer_angle && a < layer_angle + layer_span {
                                return Some(ChartHit::ImageLayer(i, j));
                            }
                            layer_angle += layer_span;
                        }
                    }
                    child += item_span;
                }
            }
            return None;
        }
        if radius >= r * 0.63 && radius <= r {
            let mut child = angle;
            for (i, item) in chart_items(items, group) {
                let bytes = item.chart_bytes;
                if bytes == 0 {
                    continue;
                }
                let part = span * bytes as f32 / subtotal as f32;
                if a >= child && a < child + part {
                    return Some(ChartHit::Resource(i));
                }
                child += part;
            }
        }
        angle += span;
    }
    None
}

fn safe_docker(args: &[&str]) -> Result<String> {
    docker(args).map_err(|_|anyhow::anyhow!("Docker ha rifiutato la rimozione. La risorsa può essere in uso o lo stato è cambiato; aggiorna la scansione e ricontrolla."))
}
fn delete_resource(resource: &Resource) -> Result<(), String> {
    let result = (|| -> Result<()> {
        match resource.kind.as_str() {
            "Container" => {
                let c: Vec<Container> = inspect(std::slice::from_ref(&resource.docker_ref))
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "Non riesco a verificare lo stato del container. Aggiorna la scansione."
                        )
                    })?;
                if c.first()
                    .and_then(|x| x.state.as_ref())
                    .is_some_and(|s| s.status.eq_ignore_ascii_case("running"))
                {
                    anyhow::bail!(
                        "Il container è in esecuzione. Arrestalo e ripeti la scansione prima di eliminarlo."
                    )
                }
                safe_docker(&["container", "rm", &resource.docker_ref])?;
            }
            "Immagine" => {
                let cids = ids(&["container", "ls", "-aq"]).map_err(|_| {
                    anyhow::anyhow!("Non riesco a verificare i container. Aggiorna la scansione.")
                })?;
                let containers: Vec<Container> = inspect(&cids).map_err(|_| {
                    anyhow::anyhow!(
                        "Non riesco a verificare l'uso dell'immagine. Aggiorna la scansione."
                    )
                })?;
                if containers.iter().any(|c| c.image_id == resource.docker_ref) {
                    anyhow::bail!(
                        "Immagine usata da uno o più container, anche arrestati. Rimuovi o ricrea i container e riprova."
                    )
                }
                safe_docker(&["image", "rm", &resource.docker_ref])?;
            }
            "Volume" => {
                let cids = ids(&["container", "ls", "-aq"]).map_err(|_| {
                    anyhow::anyhow!("Non riesco a verificare i container. Aggiorna la scansione.")
                })?;
                let containers: Vec<Container> = inspect(&cids).map_err(|_| {
                    anyhow::anyhow!(
                        "Non riesco a verificare l'uso del volume. Aggiorna la scansione."
                    )
                })?;
                if containers.iter().any(|c| {
                    c.mounts.iter().any(|m| {
                        m.kind == "volume" && m.name.trim_start_matches('/') == resource.docker_ref
                    })
                }) {
                    anyhow::bail!(
                        "Volume ancora collegato a uno o più container, anche arrestati. Rimuovi o ricrea i container e riprova."
                    )
                }
                safe_docker(&["volume", "rm", &resource.docker_ref])?;
            }
            _ => anyhow::bail!("Tipo di risorsa Docker non supportato."),
        }
        Ok(())
    })();
    result.map_err(|e| e.to_string())
}
fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180., 760.])
            .with_min_inner_size([850., 560.]),
        ..Default::default()
    };
    eframe::run_native("Docker Daisy", opts, Box::new(|_| Ok(Box::new(App::new()))))
}
