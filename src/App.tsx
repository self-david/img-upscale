import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ask, open, save } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/// Las imágenes guardadas en SQLite se sirven con el protocolo propio de Tauri.
const APPIMG_BASE = /Windows/i.test(navigator.userAgent)
  ? "http://appimg.localhost"
  : "appimg://localhost";
const resultSrc = (id: string, thumb = false) =>
  isTauri ? `${APPIMG_BASE}/${thumb ? "thumb/" : ""}${id}` : "";

type Level = "simple" | "intermedio" | "potente";
type Section = "escalar" | "resultados" | "buscar";

const LEVELS: Array<{ id: Level; name: string; detail: string; time: string }> = [
  {
    id: "simple",
    name: "Simple",
    detail: "Lanczos · sin GPU, interpola (no añade detalle)",
    time: "1–2 s",
  },
  {
    id: "intermedio",
    name: "Intermedio",
    detail: "IA general (general-x4v3) · rápida",
    time: "3–8 s",
  },
  { id: "potente", name: "Potente", detail: "IA general (x4plus) · máxima calidad", time: "10–25 s" },
];

type JobStatus = "pending" | "running" | "done" | "error" | "cancelled";

type ImageMeta = {
  path: string;
  name: string;
  width: number;
  height: number;
  size: number;
};

type Item = ImageMeta & {
  id: string;
  status: JobStatus;
  progress: number;
  stage: string;
  /// Ajustes con los que se generó el resultado actual (nivel:factor:fondo).
  recipe?: string;
  resultId?: string;
  outWidth?: number;
  outHeight?: number;
  message?: string;
  ms?: number;
};

type StoredResult = {
  id: string;
  name: string;
  format: string;
  width: number;
  height: number;
  scale: number;
  level: Level;
  size: number;
  createdAt: number;
};

const levelLabel = (id: string) => LEVELS.find((entry) => entry.id === id)?.name ?? id;

type SearchHit = {
  id: string;
  thumb: string;
  url: string;
  page: string | null;
  title: string;
  author: string | null;
  license: string | null;
  width: number;
  height: number;
  source: string;
};

type HitStatus = {
  id: string;
  state: "ok" | "empty" | "no-key" | "off" | "error";
  count: number;
  message: string | null;
};

type SourceInfo = {
  id: string;
  name: string;
  needsKey: "brave" | "serpapi" | "searxng" | null;
  configured: boolean;
  enabled: boolean;
  signup: string;
  note: string;
};

type SearchFilters = { size: string; orientation: string; openLicense: boolean };

const STATUS_CHIPS: Record<JobStatus, { label: string; className: string }> = {
  pending: { label: "En espera", className: "bg-white/[0.06] text-zinc-400" },
  running: { label: "Procesando", className: "bg-indigo-500/15 text-indigo-300" },
  done: { label: "Listo", className: "bg-emerald-500/15 text-emerald-300" },
  error: { label: "Error", className: "bg-rose-500/15 text-rose-300" },
  cancelled: { label: "Cancelada", className: "bg-amber-500/15 text-amber-300" },
};

const ICONS = {
  spark: "M12 3l1.9 4.7L18.5 9.5l-4.6 1.8L12 16l-1.9-4.7L5.5 9.5l4.6-1.8zM18.5 15.5l.8 2 2 .8-2 .8-.8 2-.8-2-2-.8 2-.8z",
  image: "M4 5h16v14H4zM9 11a1.6 1.6 0 100-3.2A1.6 1.6 0 009 11zM4 16.5l5-4.5 4 3.5 3-2.5 4 3.5",
  folder: "M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2z",
  plus: "M12 5v14M5 12h14",
  x: "M6 6l12 12M18 6L6 18",
  check: "M20 6L9 17l-5-5",
  alert: "M12 9v4M12 17h.01M12 3.5l9 16.5H3z",
  monitor: "M3 5h18v11H3zM9 20h6M12 16v4",
  search: "M11 4a7 7 0 105.2 11.7L21 21M11 4a7 7 0 00-2 13.7",
  globe: "M12 3a9 9 0 100 18 9 9 0 000-18zM3 12h18M12 3c2.5 2.6 3.8 5.7 3.8 9S14.5 18.4 12 21c-2.5-2.6-3.8-5.7-3.8-9S9.5 5.6 12 3z",
  layers: "M12 3l9 5-9 5-9-5 9-5zM3 13l9 5 9-5",
  compare: "M12 4v16M5 8l-3 4 3 4M19 8l3 4-3 4",
  grid: "M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z",
  copy: "M9 9h10v10H9zM15 9V5H5v10h4",
  download: "M12 4v10M8 10l4 4 4-4M5 19h14",
};

function cx(...parts: Array<string | false | null | undefined>) {
  return parts.filter(Boolean).join(" ");
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toLocaleString("es-ES", { maximumFractionDigits: value < 10 ? 1 : 0 })} ${units[unit]}`;
}

const formatNumber = (value: number, digits = 1) =>
  value.toLocaleString("es-ES", { maximumFractionDigits: digits });

function Icon({ d, className = "h-4 w-4" }: { d: string; className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden="true"
    >
      <path d={d} />
    </svg>
  );
}

export default function App() {
  const [items, setItems] = useState<Item[]>([]);
  const [scale, setScale] = useState(2);
  const [level, setLevel] = useState<Level>("potente");
  const [removeBg, setRemoveBg] = useState(false);
  const [section, setSection] = useState<Section>("escalar");
  const [query, setQuery] = useState("");
  const [searchHint, setSearchHint] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [hitStatuses, setHitStatuses] = useState<HitStatus[]>([]);
  const [sources, setSources] = useState<SourceInfo[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchPage, setSearchPage] = useState(0);
  const [searched, setSearched] = useState(false);
  const [filters, setFilters] = useState<SearchFilters>({
    size: "any",
    orientation: "any",
    openLicense: false,
  });
  const [visualQuery, setVisualQuery] = useState<string | null>(null);
  const [busyHit, setBusyHit] = useState<string | null>(null);
  const dropRef = useRef<((paths: string[]) => void) | null>(null);
  const sectionRef = useRef<Section>("escalar");
  sectionRef.current = section;
  const [library, setLibrary] = useState<StoredResult[]>([]);
  const [selectedResult, setSelectedResult] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [notice, setNotice] = useState("");
  const [comparing, setComparing] = useState<Item | null>(null);

  const updateItem = useCallback((id: string, patch: Partial<Item>) => {
    setItems((previous) =>
      previous.map((item) => (item.id === id ? { ...item, ...patch } : item)),
    );
  }, []);

  const addPaths = useCallback(async (paths: string[]) => {
    if (paths.length === 0) return;
    if (!isTauri) {
      setNotice("La cola funciona dentro de la app de escritorio (no en el navegador).");
      return;
    }
    try {
      const metas = await invoke<ImageMeta[]>("add_paths", { paths });
      setItems((previous) => {
        const known = new Set(previous.map((item) => item.path.toLowerCase()));
        const fresh = metas
          .filter((meta) => !known.has(meta.path.toLowerCase()))
          .map<Item>((meta) => ({
            ...meta,
            id: crypto.randomUUID(),
            status: "pending",
            progress: 0,
            stage: "",
          }));
        return [...previous, ...fresh];
      });
      if (metas.length === 0) {
        setNotice("No se encontraron imágenes compatibles en esa selección.");
      }
    } catch (error) {
      setNotice(`No se pudieron añadir las imágenes: ${String(error)}`);
    }
  }, []);

  const refreshLibrary = useCallback(async () => {
    if (!isTauri) return;
    try {
      setLibrary(await invoke<StoredResult[]>("list_results"));
    } catch (error) {
      setNotice(`No se pudo leer la biblioteca: ${String(error)}`);
    }
  }, []);

  useEffect(() => {
    void refreshLibrary();
  }, [refreshLibrary]);

  const copyResult = useCallback(async (result: StoredResult) => {
    try {
      await invoke("copy_result", { id: result.id });
      setNotice(`«${result.name}» copiada al portapapeles`);
    } catch (error) {
      setNotice(String(error));
    }
  }, []);

  const downloadResult = useCallback(async (result: StoredResult) => {
    try {
      const extension = result.format === "jpg" ? "jpg" : result.format;
      const target = await save({
        title: "Guardar imagen",
        defaultPath: result.name,
        filters: [{ name: "Imagen", extensions: [extension] }],
      });
      if (!target) return;
      await invoke("export_result", { id: result.id, path: target });
      setNotice(`Guardada en ${target}`);
    } catch (error) {
      setNotice(String(error));
    }
  }, []);

  const deleteResult = useCallback(
    async (result: StoredResult) => {
      try {
        const sure = await ask(`¿Eliminar «${result.name}» de la biblioteca?`, {
          title: "Eliminar imagen",
          kind: "warning",
          okLabel: "Eliminar",
          cancelLabel: "Cancelar",
        });
        if (!sure) return;
        setLibrary(await invoke<StoredResult[]>("delete_result", { id: result.id }));
        setSelectedResult((current) => (current === result.id ? null : current));
        setNotice("Imagen eliminada de la biblioteca.");
      } catch (error) {
        setNotice(String(error));
      }
    },
    [],
  );

  useEffect(() => {
    if (!isTauri || section !== "resultados" || !selectedResult) return;
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey) || event.key.toLowerCase() !== "c") return;
      const target = library.find((entry) => entry.id === selectedResult);
      if (!target) return;
      event.preventDefault();
      void copyResult(target);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [section, selectedResult, library, copyResult]);

  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(""), 5000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  const refreshSources = useCallback(async () => {
    if (!isTauri) return;
    try {
      setSources(await invoke<SourceInfo[]>("search_sources"));
    } catch (error) {
      setNotice(`No se pudieron cargar las fuentes: ${String(error)}`);
    }
  }, []);

  useEffect(() => {
    void refreshSources();
  }, [refreshSources]);

  const runSearch = async (page = 0) => {
    const text = query.trim();
    if (!isTauri) {
      setNotice("La búsqueda funciona dentro de la app de escritorio.");
      return;
    }
    if (!text) {
      setSearchHint("Escribe qué imagen buscas.");
      return;
    }
    setSearching(true);
    setSearchHint("");
    try {
      const response = await invoke<{
        hits: SearchHit[];
        statuses: HitStatus[];
        tookMs: number;
      }>("search_images", { request: { query: text, page, filters } });
      setHits((previous) => (page === 0 ? response.hits : [...previous, ...response.hits]));
      setHitStatuses(response.statuses);
      setSearchPage(page);
      setSearched(true);
      if (response.hits.length === 0) {
        setSearchHint("Sin resultados con esos filtros. Prueba otra búsqueda.");
      }
    } catch (error) {
      setSearchHint(String(error));
    } finally {
      setSearching(false);
    }
  };

  const searchByImage = async (path: string) => {
    setVisualQuery(path);
    setSearchHint("");
    setSearching(true);
    try {
      const response = await invoke<{ hits: SearchHit[]; statuses: HitStatus[] }>(
        "reverse_image_search",
        { path },
      );
      setHits(response.hits);
      setHitStatuses(response.statuses);
      setSearched(true);
      setSearchPage(0);
    } catch (error) {
      setSearchHint(String(error));
    } finally {
      setSearching(false);
    }
  };

  const addHit = async (hit: SearchHit) => {
    setBusyHit(hit.id);
    try {
      const meta = await invoke<ImageMeta>("download_image", {
        url: hit.url,
        referer: hit.page,
      });
      await addPaths([meta.path]);
      setNotice(`Añadida a la cola: ${meta.name}`);
    } catch (error) {
      setNotice(String(error));
    } finally {
      setBusyHit(null);
    }
  };

  const handleDrop = (paths: string[]) => {
    if (sectionRef.current === "buscar") {
      const image = paths.find((path) => /\.(png|jpe?g|webp|bmp|tiff?|gif)$/i.test(path));
      if (image) void searchByImage(image);
      else setSearchHint("Suelta un archivo de imagen para buscarla.");
      return;
    }
    void addPaths(paths);
  };
  dropRef.current = handleDrop;

  useEffect(() => {
    if (!isTauri) return;
    let disposed = false;
    let stopDrag: (() => void) | undefined;

    const subscriptions = Promise.all([
      listen<{ id: string; percent: number; stage: string }>("job:progress", ({ payload }) =>
        updateItem(payload.id, {
          status: "running",
          progress: payload.percent,
          stage: payload.stage,
        }),
      ),
      listen<{
        id: string;
        resultId: string;
        width: number;
        height: number;
        ms: number;
      }>("job:done", ({ payload }) => {
        updateItem(payload.id, {
          status: "done",
          progress: 100,
          stage: "Listo",
          resultId: payload.resultId,
          outWidth: payload.width,
          outHeight: payload.height,
          ms: payload.ms,
        });
        void refreshLibrary();
      }),
      listen<{ id: string; message: string }>("job:error", ({ payload }) =>
        updateItem(payload.id, { status: "error", progress: 0, message: payload.message }),
      ),
      listen<{ id: string }>("job:cancelled", ({ payload }) =>
        updateItem(payload.id, { status: "cancelled", progress: 0, stage: "Cancelada" }),
      ),
      listen("batch:done", () => {
        setRunning(false);
        void refreshLibrary();
      }),
    ]);

    const attach = async () => {
      const unlisteners = await subscriptions;
      if (disposed) unlisteners.forEach((unlisten) => unlisten());
      else
        stopDrag = await getCurrentWebview()
          .onDragDropEvent((event) => {
            if (event.payload.type === "drop") {
              setDragging(false);
              dropRef.current?.(event.payload.paths);
            } else {
              setDragging(event.payload.type === "over");
            }
          })
          .catch(() => undefined);
    };

    void attach();

    return () => {
      disposed = true;
      void subscriptions.then((unlisteners) => unlisteners.forEach((unlisten) => unlisten()));
      stopDrag?.();
    };
  }, [addPaths, refreshLibrary, updateItem]);

  const choose = async (options: Parameters<typeof open>[0]) => {
    try {
      return await open(options);
    } catch (error) {
      setNotice(String(error));
      return null;
    }
  };

  const addFromPicker = async (folder: boolean) => {
    const selection = await choose(
      folder
        ? { directory: true, multiple: false, title: "Elige una carpeta" }
        : {
            multiple: true,
            title: "Elige imágenes",
            filters: [
              {
                name: "Imágenes",
                extensions: ["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff", "gif"],
              },
            ],
          },
    );
    if (!selection) return;
    await addPaths(Array.isArray(selection) ? selection : [selection]);
  };

  /// Ajustes actuales: si cambian, las imágenes ya escaladas vuelven a la cola.
  const recipe = `${level}:${scale}:${removeBg ? 1 : 0}`;

  const pending = useMemo(
    () =>
      items.filter(
        (item) =>
          item.status === "pending" ||
          item.status === "error" ||
          item.status === "cancelled" ||
          (item.status === "done" && item.recipe !== recipe),
      ),
    [items, recipe],
  );

  const totalMp = items.reduce((sum, item) => sum + (item.width * item.height) / 1_000_000, 0);
  const completed = items.filter((item) => item.status === "done").length;
  const libraryBytes = library.reduce((sum, result) => sum + result.size, 0);

  const start = async () => {
    if (pending.length === 0 || running) return;
    setRunning(true);
    const ids = new Set(pending.map((item) => item.id));
    setItems((previous) =>
      previous.map((item) =>
        ids.has(item.id)
          ? {
              ...item,
              status: "pending",
              progress: 0,
              stage: "En cola",
              message: undefined,
              recipe,
            }
          : item,
      ),
    );
    try {
      await invoke("start_batch", {
        jobs: pending.map((item) => ({ id: item.id, path: item.path })),
        scale,
        level,
        removeBg,
      });
    } catch (error) {
      setNotice(String(error));
      setRunning(false);
    }
  };

  const cancel = async () => {
    try {
      await invoke("cancel_batch");
    } catch (error) {
      setNotice(String(error));
    }
  };

  return (
    <div className="flex h-full min-h-0 w-full overflow-hidden bg-[#08090d] text-zinc-200">
      <aside className="flex w-[338px] shrink-0 flex-col border-r border-white/[0.06] bg-[#0b0d13]">
        <div className="flex items-center gap-3 px-6 pt-6 pb-5">
          <div className="grid h-10 w-10 place-items-center rounded-2xl bg-gradient-to-br from-indigo-400 via-violet-500 to-fuchsia-500 text-white shadow-lg shadow-indigo-500/25">
            <Icon d={ICONS.spark} className="h-5 w-5" />
          </div>
          <div className="min-w-0">
            <p className="text-[15px] font-semibold tracking-tight text-white">Upscaler</p>
            <p className="truncate text-[11px] text-zinc-500">Escalado local · v0.3</p>
          </div>
        </div>

        <div className="px-6 pb-4">
          <div className="flex gap-1 rounded-xl border border-white/[0.06] bg-white/[0.02] p-1">
            {[
              { id: "escalar" as const, label: "Escalar", icon: ICONS.layers },
              { id: "resultados" as const, label: "Resultados", icon: ICONS.grid },
              { id: "buscar" as const, label: "Buscar", icon: ICONS.search },
            ].map((option) => (
              <button
                key={option.id}
                type="button"
                onClick={() => setSection(option.id)}
                className={cx(
                  "flex flex-1 items-center justify-center gap-1.5 rounded-lg py-1.5 text-[12px] font-medium transition",
                  section === option.id
                    ? "bg-white/[0.09] text-white shadow-sm"
                    : "text-zinc-500 hover:text-zinc-300",
                )}
              >
                <Icon d={option.icon} className="h-3.5 w-3.5" />
                {option.label}
              </button>
            ))}
          </div>
        </div>

        <div
          className={cx(
            "min-h-0 flex-1 space-y-6 overflow-y-auto px-6 pb-6",
            section !== "escalar" && "hidden",
          )}
        >
          <section>
            <p className="mb-2.5 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-500">
              Modo
            </p>
            <div className="space-y-2">
              {LEVELS.map((option) => {
                const active = level === option.id;
                return (
                  <button
                    key={option.id}
                    type="button"
                    onClick={() => setLevel(option.id)}
                    className={cx(
                      "w-full rounded-xl border p-3 text-left transition",
                      active
                        ? "border-indigo-400/40 bg-indigo-500/[0.09]"
                        : "border-white/[0.06] bg-white/[0.015] hover:border-white/15 hover:bg-white/[0.03]",
                    )}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-[13px] font-medium text-zinc-100">{option.name}</span>
                      <span className="flex items-center gap-1.5 text-[10px] text-zinc-500">
                        {option.time}
                        <span
                          className={cx(
                            "grid h-4 w-4 place-items-center rounded-full",
                            active
                              ? "bg-indigo-400 text-[#0b0d13]"
                              : "border border-white/15 text-transparent",
                          )}
                        >
                          <Icon d={ICONS.check} className="h-2.5 w-2.5" />
                        </span>
                      </span>
                    </div>
                    <p className="mt-0.5 text-[11px] text-zinc-500">{option.detail}</p>
                  </button>
                );
              })}
            </div>
            <p className="mt-2 text-[10px] leading-relaxed text-zinc-600">
              La IA trabaja a 4× y luego ajusta al factor elegido. Tiempos orientativos para 12 MP a
              2×; cada imagen muestra su tiempo real al terminar.
            </p>
          </section>

          <section>
            <p className="mb-2.5 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-500">
              Factor
            </p>
            <div className="flex gap-1 rounded-xl border border-white/[0.06] bg-white/[0.02] p-1">
              {[2, 4, 8].map((factor) => (
                <button
                  key={factor}
                  type="button"
                  onClick={() => setScale(factor)}
                  className={cx(
                    "flex-1 rounded-lg py-1.5 text-[12px] font-medium transition",
                    scale === factor
                      ? "bg-white/[0.09] text-white shadow-sm"
                      : "text-zinc-500 hover:text-zinc-300",
                  )}
                >
                  {factor}×
                </button>
              ))}
            </div>
            <p className="mt-2 text-[11px] leading-relaxed text-zinc-500">
              {scale === 8
                ? "8× parte del 4× de la IA: el detalle extra es interpolado."
                : `Cada imagen sale al ${scale * 100}% de su tamaño original.`}
            </p>
          </section>

          <section>
            <p className="mb-2.5 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-500">
              Después del escalado
            </p>
            <button
              type="button"
              onClick={() => setRemoveBg(!removeBg)}
              aria-pressed={removeBg}
              className={cx(
                "flex w-full items-center justify-between gap-3 rounded-xl border p-3 text-left transition",
                removeBg
                  ? "border-indigo-400/40 bg-indigo-500/[0.09]"
                  : "border-white/[0.06] bg-white/[0.015] hover:border-white/15 hover:bg-white/[0.03]",
              )}
            >
              <span className="min-w-0">
                <span className="text-[13px] font-medium text-zinc-100">Quitar el fondo</span>
                <span className="mt-0.5 block text-[11px] leading-relaxed text-zinc-500">
                  Se ejecuta al terminar el escalado y guarda un PNG con transparencia
                </span>
              </span>
              <span
                className={cx(
                  "relative h-5 w-9 shrink-0 rounded-full transition",
                  removeBg ? "bg-indigo-500" : "bg-white/15",
                )}
              >
                <span
                  className={cx(
                    "absolute top-0.5 h-4 w-4 rounded-full bg-white transition-all",
                    removeBg ? "left-[18px]" : "left-0.5",
                  )}
                />
              </span>
            </button>
          </section>

          <p className="rounded-xl border border-white/[0.06] bg-white/[0.02] p-3 text-[11px] leading-relaxed text-zinc-500">
            Los resultados se guardan en la biblioteca SQLite de la app. Desde «Resultados» puedes
            copiarlos al portapapeles, guardarlos en el disco o eliminarlos.
          </p>
        </div>

        <div
          className={cx(
            "min-h-0 flex-1 space-y-4 overflow-y-auto px-6 pb-6",
            section !== "buscar" && "hidden",
          )}
        >
          <section>
            <p className="mb-2.5 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-500">
              Fuentes
            </p>
            <p className="mb-3 text-[11px] leading-relaxed text-zinc-500">
              Se consultan todas a la vez y los resultados se mezclan quitando duplicados.
            </p>
            <div className="space-y-2">
              {sources.map((source) => (
                <SourceRow
                  key={source.id}
                  source={source}
                  onSaved={setSources}
                />
              ))}
              {sources.length === 0 && (
                <p className="text-[11px] text-zinc-600">Cargando fuentes…</p>
              )}
            </div>
          </section>
          <p className="rounded-xl border border-white/[0.06] bg-white/[0.02] p-3 text-[11px] leading-relaxed text-zinc-500">
            Suelta una imagen en el área de resultados para buscar parecidas. Esa búsqueda sube la
            imagen al proveedor de búsqueda visual (hoy SerpAPI).
          </p>
        </div>

        <div
          className={cx(
            "min-h-0 flex-1 space-y-4 overflow-y-auto px-6 pb-6",
            section !== "resultados" && "hidden",
          )}
        >
          <section>
            <p className="mb-2.5 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-500">
              Biblioteca
            </p>
            <p className="text-[11px] leading-relaxed text-zinc-500">
              Las imágenes escaladas se guardan en una base de datos SQLite dentro de la app. Desde
              aquí puedes copiarlas al portapapeles, guardarlas en el disco o eliminarlas.
            </p>
            <dl className="mt-3 space-y-1.5 rounded-xl border border-white/[0.06] bg-white/[0.02] p-3 text-[11px]">
              <div className="flex items-center justify-between">
                <dt className="text-zinc-500">Imágenes</dt>
                <dd className="text-zinc-200">{library.length}</dd>
              </div>
              <div className="flex items-center justify-between">
                <dt className="text-zinc-500">Espacio</dt>
                <dd className="text-zinc-200">{formatBytes(libraryBytes)}</dd>
              </div>
            </dl>
          </section>
          <p className="rounded-xl border border-white/[0.06] bg-white/[0.02] p-3 text-[11px] leading-relaxed text-zinc-500">
            Haz clic en una imagen para seleccionarla y pulsa <kbd className="rounded bg-white/[0.08] px-1 py-0.5 text-[10px] text-zinc-300">Ctrl</kbd>{" "}
            + <kbd className="rounded bg-white/[0.08] px-1 py-0.5 text-[10px] text-zinc-300">C</kbd> para copiarla al portapapeles.
          </p>
        </div>

        <div
          className={cx(
            "border-t border-white/[0.06] px-6 py-5",
            section !== "escalar" && "hidden",
          )}
        >
          {running ? (
            <button
              type="button"
              onClick={cancel}
              className="flex w-full items-center justify-center gap-2 rounded-xl border border-rose-400/30 bg-rose-500/10 px-4 py-3 text-[13px] font-semibold text-rose-200 transition hover:bg-rose-500/20"
            >
              <span className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-rose-300/30 border-t-rose-200" />
              Cancelar
            </button>
          ) : (
            <button
              type="button"
              onClick={start}
              disabled={pending.length === 0}
              className={cx(
                "flex w-full items-center justify-center gap-2 rounded-xl px-4 py-3 text-[13px] font-semibold transition",
                pending.length === 0
                  ? "cursor-not-allowed bg-white/[0.04] text-zinc-500"
                  : "bg-gradient-to-r from-indigo-500 to-fuchsia-500 text-white shadow-lg shadow-indigo-500/25 hover:from-indigo-400 hover:to-fuchsia-400",
              )}
            >
              Procesar tanda
              {pending.length > 0 && (
                <span className="rounded-full bg-black/20 px-1.5 py-0.5 text-[11px]">
                  {pending.length}
                </span>
              )}
            </button>
          )}
          <p className="mt-2.5 text-center text-[10px] leading-relaxed text-zinc-600">
            Sin subidas a internet. Tus imágenes no salen del equipo.
          </p>
        </div>
      </aside>

      <main className="relative flex min-w-0 flex-1 flex-col">
        {section === "buscar" ? (
          <SearchView
            query={query}
            onQuery={(value) => {
              setQuery(value);
              setSearchHint("");
            }}
            hint={searchHint}
            onSubmit={() => void runSearch(0)}
            hits={hits}
            statuses={hitStatuses}
            sourceNames={sources}
            searching={searching}
            searched={searched}
            filters={filters}
            onFilters={setFilters}
            onAdd={(hit) => void addHit(hit)}
            busyHit={busyHit}
            onMore={() => void runSearch(searchPage + 1)}
            visualQuery={visualQuery}
            onClearVisual={() => {
              setVisualQuery(null);
              setHits([]);
              setHitStatuses([]);
              setSearched(false);
            }}
          />
        ) : section === "resultados" ? (
          <ResultsView
            results={library}
            selected={selectedResult}
            onSelect={setSelectedResult}
            onCopy={(result) => void copyResult(result)}
            onDownload={(result) => void downloadResult(result)}
            onDelete={(result) => void deleteResult(result)}
          />
        ) : (
          <>
        <header className="flex items-center justify-between gap-4 border-b border-white/[0.06] px-8 py-5">
          <div className="min-w-0">
            <h1 className="text-[15px] font-semibold tracking-tight text-white">
              Cola de imágenes
            </h1>
            <p className="truncate text-[11px] text-zinc-500">
              {items.length === 0
                ? "Sin imágenes todavía"
                : [
                    `${items.length} ${items.length === 1 ? "imagen" : "imágenes"}`,
                    `${formatNumber(totalMp)} MP en total`,
                    completed > 0 ? `${completed} listas` : null,
                  ]
                    .filter(Boolean)
                    .join(" · ")}
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <button
              type="button"
              onClick={() => addFromPicker(true)}
              className="rounded-lg border border-white/[0.08] bg-white/[0.02] px-3 py-2 text-[12px] text-zinc-300 transition hover:border-white/15 hover:bg-white/[0.05]"
            >
              Añadir carpeta
            </button>
            <button
              type="button"
              onClick={() => addFromPicker(false)}
              className="flex items-center gap-1.5 rounded-lg bg-white px-3 py-2 text-[12px] font-medium text-[#0b0d13] transition hover:bg-zinc-200"
            >
              <Icon d={ICONS.plus} className="h-3.5 w-3.5" />
              Añadir imágenes
            </button>
          </div>
        </header>

        <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">
          {items.length === 0 ? (
            <div className="flex h-full min-h-[320px] flex-col items-center justify-center rounded-2xl border border-dashed border-white/[0.09] bg-white/[0.012] px-8 py-12 text-center">
              <div className="grid h-14 w-14 place-items-center rounded-2xl border border-white/[0.08] bg-white/[0.03] text-zinc-400">
                <Icon d={ICONS.image} className="h-6 w-6" />
              </div>
              <p className="mt-5 text-[15px] font-medium text-zinc-100">
                Arrastra tus imágenes aquí
              </p>
              <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-zinc-500">
                También puedes soltar una carpeta: se añaden solo los archivos de su interior, sin
                entrar en subcarpetas.
              </p>
              <div className="mt-6 flex items-center gap-2">
                <button
                  type="button"
                  onClick={() => addFromPicker(false)}
                  className="rounded-lg bg-white px-3.5 py-2 text-[12px] font-medium text-[#0b0d13] transition hover:bg-zinc-200"
                >
                  Elegir imágenes
                </button>
                <button
                  type="button"
                  onClick={() => addFromPicker(true)}
                  className="rounded-lg border border-white/[0.08] px-3.5 py-2 text-[12px] text-zinc-300 transition hover:border-white/15 hover:bg-white/[0.05]"
                >
                  Elegir carpeta
                </button>
              </div>
              <p className="mt-6 text-[11px] text-zinc-600">JPG · PNG · WebP · BMP · TIFF</p>
            </div>
          ) : (
            <div className="space-y-2">
              {items.map((item) => {
                const chip = STATUS_CHIPS[item.status];
                const outWidth = item.outWidth ?? item.width * scale;
                const outHeight = item.outHeight ?? item.height * scale;
                const stored = item.resultId
                  ? library.find((entry) => entry.id === item.resultId)
                  : undefined;
                return (
                  <div
                    key={item.id}
                    className="group flex items-center gap-4 rounded-xl border border-white/[0.06] bg-white/[0.018] p-3 transition hover:border-white/[0.12] hover:bg-white/[0.03]"
                  >
                    <div className="flex shrink-0 items-center gap-1.5">
                      <Preview
                        src={isTauri ? convertFileSrc(item.path) : undefined}
                        label="antes"
                        onClick={item.resultId ? () => setComparing(item) : undefined}
                      />
                      {item.resultId && (
                        <Preview
                          src={resultSrc(item.resultId, true)}
                          label="después"
                          accent
                          onClick={() => setComparing(item)}
                        />
                      )}
                    </div>

                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <p className="truncate text-[13px] font-medium text-zinc-100">
                          {item.name}
                        </p>
                        <span
                          className={cx(
                            "shrink-0 rounded-full px-2 py-0.5 text-[10px] font-medium tracking-wide",
                            chip.className,
                          )}
                        >
                          {chip.label}
                        </span>
                      </div>
                      <p className="mt-0.5 truncate text-[11px] text-zinc-500">
                        {item.width}×{item.height} · {formatBytes(item.size)} → {outWidth}×
                        {outHeight}
                        {item.status === "done" && item.ms
                          ? ` · ${formatNumber(item.ms / 1000)} s`
                          : ""}
                      </p>

                      {item.status === "done" && item.recipe !== recipe && (
                        <p className="mt-1 text-[10px] text-amber-200/80">
                          Ajustes cambiados: se volverá a escalar al procesar la tanda.
                        </p>
                      )}

                      {item.status === "running" && (
                        <div className="mt-2 flex items-center gap-2">
                          <div className="h-1 flex-1 overflow-hidden rounded-full bg-white/[0.08]">
                            <div
                              className="h-full rounded-full bg-gradient-to-r from-indigo-400 to-fuchsia-400 transition-[width] duration-300"
                              style={{ width: `${Math.max(4, item.progress)}%` }}
                            />
                          </div>
                          <span className="w-16 shrink-0 text-right text-[10px] text-zinc-500">
                            {item.stage}
                          </span>
                        </div>
                      )}

                      {item.status === "error" && item.message && (
                        <p className="mt-1.5 flex items-start gap-1.5 text-[11px] leading-relaxed text-rose-300/90">
                          <Icon d={ICONS.alert} className="mt-0.5 h-3 w-3 shrink-0" />
                          {item.message}
                        </p>
                      )}
                    </div>

                    <div className="flex shrink-0 items-center gap-1 opacity-0 transition group-hover:opacity-100 focus-within:opacity-100">
                      {stored && (
                        <>
                          <button
                            type="button"
                            title="Copiar al portapapeles"
                            onClick={() => void copyResult(stored)}
                            className="grid h-7 w-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/[0.06] hover:text-zinc-200"
                          >
                            <Icon d={ICONS.copy} className="h-3.5 w-3.5" />
                          </button>
                          <button
                            type="button"
                            title="Guardar una copia"
                            onClick={() => void downloadResult(stored)}
                            className="grid h-7 w-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/[0.06] hover:text-zinc-200"
                          >
                            <Icon d={ICONS.download} className="h-3.5 w-3.5" />
                          </button>
                        </>
                      )}
                      <button
                        type="button"
                        title="Quitar de la cola"
                        disabled={item.status === "running"}
                        onClick={() =>
                          setItems((previous) => previous.filter((entry) => entry.id !== item.id))
                        }
                        className="grid h-7 w-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/[0.06] hover:text-rose-300 disabled:opacity-30"
                      >
                        <Icon d={ICONS.x} className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        <footer className="flex items-center justify-between gap-4 border-t border-white/[0.06] px-8 py-3.5 text-[11px] text-zinc-500">
          <span className="flex items-center gap-1.5">
            <Icon d={ICONS.monitor} className="h-3.5 w-3.5" />
            {running ? "Procesando en segundo plano…" : "Listo para trabajar"}
          </span>
          <button
            type="button"
            onClick={() => setSection("resultados")}
            className="rounded-md px-2 py-1 transition hover:bg-white/[0.06] hover:text-zinc-200"
          >
            Ver resultados{library.length > 0 ? ` (${library.length})` : ""}
          </button>
        </footer>
          </>
        )}

        {dragging && (
          <div className="pointer-events-none absolute inset-3 z-20 grid place-items-center rounded-2xl border-2 border-dashed border-indigo-400/60 bg-[#08090d]/85 backdrop-blur-sm">
            <div className="flex flex-col items-center gap-3">
              <div className="grid h-12 w-12 place-items-center rounded-2xl bg-indigo-500/20 text-indigo-300">
                <Icon d={section === "buscar" ? ICONS.search : ICONS.image} className="h-5 w-5" />
              </div>
              <p className="text-[13px] font-medium text-indigo-200">
                {section === "buscar"
                  ? "Suelta para buscar imágenes parecidas"
                  : "Suelta para añadir a la cola"}
              </p>
            </div>
          </div>
        )}

        {notice && (
          <div className="absolute bottom-16 left-1/2 z-30 max-w-md -translate-x-1/2 rounded-xl border border-white/[0.08] bg-[#14161d] px-4 py-2.5 text-[12px] text-zinc-300 shadow-2xl">
            {notice}
          </div>
        )}

        {comparing?.resultId && (
          <Comparator
            item={comparing}
            outputSrc={resultSrc(comparing.resultId)}
            originalSrc={isTauri ? convertFileSrc(comparing.path) : ""}
            onClose={() => setComparing(null)}
          />
        )}
      </main>
    </div>
  );
}

const STATE_LABEL: Record<HitStatus["state"], string> = {
  ok: "resultados",
  empty: "sin resultados",
  "no-key": "sin clave",
  off: "apagada",
  error: "error",
};

const FILTERS = [
  { key: "size" as const, options: [
    { id: "any", label: "Tamaño" },
    { id: "large", label: "≥1200 px" },
    { id: "medium", label: "800–1200 px" },
  ] },
  { key: "orientation" as const, options: [
    { id: "any", label: "Forma" },
    { id: "landscape", label: "Horizontal" },
    { id: "portrait", label: "Vertical" },
    { id: "square", label: "Cuadrada" },
  ] },
];

function SearchView({
  query,
  onQuery,
  hint,
  onSubmit,
  hits,
  statuses,
  sourceNames,
  searching,
  searched,
  filters,
  onFilters,
  onAdd,
  busyHit,
  onMore,
  visualQuery,
  onClearVisual,
}: {
  query: string;
  onQuery: (value: string) => void;
  hint: string;
  onSubmit: () => void;
  hits: SearchHit[];
  statuses: HitStatus[];
  sourceNames: SourceInfo[];
  searching: boolean;
  searched: boolean;
  filters: SearchFilters;
  onFilters: (filters: SearchFilters) => void;
  onAdd: (hit: SearchHit) => void;
  busyHit: string | null;
  onMore: () => void;
  visualQuery: string | null;
  onClearVisual: () => void;
}) {
  const label = (id: string) =>
    sourceNames.find((source) => source.id === id)?.name ??
    (id === "lens" ? "Google Lens" : id);

  return (
    <>
      <header className="flex items-center justify-between gap-4 border-b border-white/[0.06] px-8 py-5">
        <div className="min-w-0">
          <h1 className="text-[15px] font-semibold tracking-tight text-white">Buscar imágenes</h1>
          <p className="truncate text-[11px] text-zinc-500">
            {visualQuery
              ? "Resultados parecidos a tu imagen"
              : "Varias fuentes a la vez, con los duplicados fuera"}
          </p>
        </div>
        <span className="shrink-0 rounded-full bg-white/[0.06] px-2.5 py-1 text-[10px] font-medium text-zinc-400">
          Beta
        </span>
      </header>

      <div className="space-y-3 border-b border-white/[0.06] px-8 py-5">
        <form
          onSubmit={(event) => {
            event.preventDefault();
            onSubmit();
          }}
          className="flex items-center gap-2"
        >
          <div className="flex min-w-0 flex-1 items-center gap-2 rounded-xl border border-white/[0.08] bg-white/[0.02] px-3 py-2.5 transition focus-within:border-indigo-400/50">
            <Icon d={ICONS.search} className="h-4 w-4 shrink-0 text-zinc-500" />
            <input
              value={query}
              onChange={(event) => onQuery(event.target.value)}
              placeholder="p. ej. «puente de piedra al atardecer»"
              className="min-w-0 flex-1 bg-transparent text-[13px] text-zinc-100 placeholder:text-zinc-600 focus:outline-none"
            />
          </div>
          <button
            type="submit"
            disabled={!query.trim() || searching}
            className={cx(
              "shrink-0 rounded-xl px-4 py-2.5 text-[12px] font-medium transition",
              query.trim() && !searching
                ? "bg-white text-[#0b0d13] hover:bg-zinc-200"
                : "cursor-not-allowed bg-white/[0.04] text-zinc-500",
            )}
          >
            {searching ? "Buscando…" : "Buscar"}
          </button>
        </form>

        <div className="flex flex-wrap items-center gap-1.5">
          {FILTERS.map((group) =>
            group.options.map((option) => {
              const active = filters[group.key] === option.id;
              return (
                <button
                  key={`${group.key}-${option.id}`}
                  type="button"
                  onClick={() => onFilters({ ...filters, [group.key]: option.id })}
                  className={cx(
                    "rounded-full px-2.5 py-1 text-[11px] transition",
                    active
                      ? "bg-white/[0.12] text-white"
                      : "bg-white/[0.03] text-zinc-500 hover:text-zinc-300",
                  )}
                >
                  {option.label}
                </button>
              );
            }),
          )}
          <button
            type="button"
            onClick={() => onFilters({ ...filters, openLicense: !filters.openLicense })}
            className={cx(
              "rounded-full px-2.5 py-1 text-[11px] transition",
              filters.openLicense
                ? "bg-emerald-500/15 text-emerald-300"
                : "bg-white/[0.03] text-zinc-500 hover:text-zinc-300",
            )}
          >
            Sólo licencias abiertas
          </button>
        </div>

        {visualQuery && (
          <div className="flex items-center gap-3 rounded-xl border border-indigo-400/30 bg-indigo-500/[0.07] p-2.5">
            <img
              src={convertFileSrc(visualQuery)}
              alt="Consulta visual"
              className="h-12 w-12 rounded-lg object-cover"
            />
            <div className="min-w-0 flex-1">
              <p className="text-[12px] font-medium text-zinc-100">Buscando por esta imagen</p>
              <p className="truncate text-[11px] text-zinc-500">{visualQuery.split("\\").pop()}</p>
            </div>
            <button
              type="button"
              onClick={onClearVisual}
              className="grid h-7 w-7 place-items-center rounded-md text-zinc-400 transition hover:bg-white/[0.06] hover:text-white"
              title="Quitar la consulta visual"
            >
              <Icon d={ICONS.x} className="h-3.5 w-3.5" />
            </button>
          </div>
        )}

        {hint && <p className="text-[11px] text-amber-200/90">{hint}</p>}

        {statuses.length > 0 && (
          <div className="flex flex-wrap gap-1.5">
            {statuses.map((status) => (
              <span
                key={status.id}
                title={status.message ?? undefined}
                className={cx(
                  "flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[10px]",
                  status.state === "ok"
                    ? "border-white/[0.08] bg-white/[0.04] text-zinc-300"
                    : status.state === "error"
                      ? "border-rose-400/20 bg-rose-500/[0.08] text-rose-300"
                      : "border-white/[0.05] bg-white/[0.015] text-zinc-500",
                )}
              >
                <span
                  className={cx(
                    "h-1.5 w-1.5 rounded-full",
                    status.state === "ok"
                      ? "bg-emerald-400"
                      : status.state === "error"
                        ? "bg-rose-400"
                        : "bg-zinc-600",
                  )}
                />
                {label(status.id)}
                <span className="text-zinc-600">
                  {status.state === "ok" ? status.count : STATE_LABEL[status.state]}
                </span>
              </span>
            ))}
          </div>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">
        {hits.length === 0 ? (
          <div className="flex h-full min-h-[320px] flex-col items-center justify-center rounded-2xl border border-dashed border-white/[0.09] bg-white/[0.012] px-8 py-12 text-center">
            <div className="grid h-14 w-14 place-items-center rounded-2xl border border-white/[0.08] bg-white/[0.03] text-zinc-400">
              <Icon d={ICONS.globe} className="h-6 w-6" />
            </div>
            <p className="mt-5 text-[15px] font-medium text-zinc-100">
              {searching
                ? "Buscando en todas las fuentes…"
                : searched
                  ? "Sin resultados"
                  : "Busca o suelta una imagen"}
            </p>
            <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-zinc-500">
              {searched
                ? "Prueba con otras palabras o quita filtros. Arriba tienes el estado de cada fuente."
                : "Escribe qué imagen necesitas, o arrastra una imagen aquí para buscar parecidas."}
            </p>
            <p className="mt-6 text-[11px] text-zinc-600">
              Openverse · Wikimedia · Brave · Google · Yandex · SearXNG
            </p>
          </div>
        ) : (
          <div className="space-y-4">
            <div className="grid grid-cols-2 gap-3 xl:grid-cols-3">
              {hits.map((hit) => (
                <HitCard
                  key={hit.id}
                  hit={hit}
                  sourceLabel={label(hit.source)}
                  busy={busyHit === hit.id}
                  onAdd={() => onAdd(hit)}
                />
              ))}
            </div>
            <button
              type="button"
              onClick={onMore}
              disabled={searching}
              className="mx-auto block rounded-xl border border-white/[0.08] px-4 py-2 text-[12px] text-zinc-300 transition hover:border-white/15 hover:bg-white/[0.04] disabled:opacity-40"
            >
              {searching ? "Buscando…" : "Cargar más resultados"}
            </button>
          </div>
        )}
      </div>
    </>
  );
}

function HitCard({
  hit,
  sourceLabel,
  busy,
  onAdd,
}: {
  hit: SearchHit;
  sourceLabel: string;
  busy: boolean;
  onAdd: () => void;
}) {
  return (
    <article className="group overflow-hidden rounded-xl border border-white/[0.06] bg-white/[0.018] transition hover:border-white/[0.12]">
      <div className="relative aspect-[4/3] overflow-hidden bg-black/40">
        <img
          src={hit.thumb}
          alt={hit.title}
          loading="lazy"
          referrerPolicy="no-referrer"
          className="h-full w-full object-cover transition group-hover:scale-[1.02]"
        />
        <span className="absolute left-2 top-2 rounded-full bg-black/65 px-2 py-0.5 text-[10px] text-zinc-200">
          {sourceLabel}
        </span>
        {hit.width > 0 && hit.height > 0 && (
          <span className="absolute right-2 top-2 rounded-full bg-black/65 px-2 py-0.5 text-[10px] text-zinc-300">
            {hit.width}×{hit.height}
          </span>
        )}
      </div>
      <div className="space-y-2 p-2.5">
        <p className="line-clamp-2 text-[12px] leading-snug text-zinc-200">{hit.title}</p>
        <p className="truncate text-[10px] text-zinc-500">
          {hit.author ?? "Autor desconocido"} · {hit.license ?? "uso sujeto a derechos"}
        </p>
        <div className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={onAdd}
            disabled={busy}
            className="flex-1 rounded-lg bg-white/[0.06] px-2 py-1.5 text-[11px] font-medium text-zinc-200 transition hover:bg-white/[0.12] disabled:opacity-50"
          >
            {busy ? "Descargando…" : "Añadir a la cola"}
          </button>
          {hit.page && (
            <button
              type="button"
              title="Abrir la página original"
              onClick={() => void openUrl(hit.page as string).catch(() => undefined)}
              className="grid h-7 w-7 place-items-center rounded-lg text-zinc-500 transition hover:bg-white/[0.06] hover:text-zinc-200"
            >
              <Icon d={ICONS.globe} className="h-3.5 w-3.5" />
            </button>
          )}
        </div>
      </div>
    </article>
  );
}

function SourceRow({
  source,
  onSaved,
}: {
  source: SourceInfo;
  onSaved: (sources: SourceInfo[]) => void;
}) {
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);

  const save = async (payload: { key?: string; enabled?: boolean }) => {
    setSaving(true);
    try {
      const next = await invoke<SourceInfo[]>("save_source", {
        id: source.id,
        key: payload.key ?? null,
        enabled: payload.enabled ?? null,
      });
      onSaved(next);
      if (payload.key) setValue("");
    } catch {
      // el error se ve en el estado de la fuente
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="rounded-xl border border-white/[0.06] bg-white/[0.015] p-2.5">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[12px] font-medium text-zinc-200">{source.name}</span>
        <button
          type="button"
          aria-pressed={source.enabled}
          onClick={() => void save({ enabled: !source.enabled })}
          disabled={saving}
          className={cx(
            "relative h-4 w-7 shrink-0 rounded-full transition disabled:opacity-50",
            source.enabled ? "bg-indigo-500" : "bg-white/15",
          )}
        >
          <span
            className={cx(
              "absolute top-0.5 h-3 w-3 rounded-full bg-white transition-all",
              source.enabled ? "left-[14px]" : "left-0.5",
            )}
          />
        </button>
      </div>
      <p className="mt-0.5 text-[10px] leading-relaxed text-zinc-500">{source.note}</p>
      {source.needsKey && (
        <div className="mt-2 flex items-center gap-1.5">
          <input
            type={source.needsKey === "searxng" ? "url" : "password"}
            value={value}
            onChange={(event) => setValue(event.target.value)}
            placeholder={
              source.configured
                ? "Configurada · pega otra para cambiarla"
                : source.needsKey === "searxng"
                  ? "https://mi-searxng"
                  : "Pega la clave"
            }
            className="min-w-0 flex-1 rounded-lg border border-white/[0.08] bg-black/20 px-2 py-1 text-[11px] text-zinc-200 placeholder:text-zinc-600 focus:border-indigo-400/50 focus:outline-none"
          />
          <button
            type="button"
            onClick={() => void save({ key: value })}
            disabled={!value.trim() || saving}
            className="rounded-lg bg-white/[0.08] px-2 py-1 text-[11px] text-zinc-200 transition hover:bg-white/[0.14] disabled:opacity-40"
          >
            Guardar
          </button>
        </div>
      )}
      <div className="mt-1.5 flex items-center justify-between text-[10px] text-zinc-600">
        <span>{source.configured ? "Lista" : "Falta configurar"}</span>
        <button
          type="button"
          onClick={() => void openUrl(source.signup).catch(() => undefined)}
          className="underline-offset-2 hover:text-zinc-300 hover:underline"
        >
          Obtener acceso
        </button>
      </div>
    </div>
  );
}

function ResultsView({
  results,
  selected,
  onSelect,
  onCopy,
  onDownload,
  onDelete,
}: {
  results: StoredResult[];
  selected: string | null;
  onSelect: (id: string) => void;
  onCopy: (result: StoredResult) => void;
  onDownload: (result: StoredResult) => void;
  onDelete: (result: StoredResult) => void;
}) {
  const total = results.reduce((sum, result) => sum + result.size, 0);

  return (
    <>
      <header className="flex items-center justify-between gap-4 border-b border-white/[0.06] px-8 py-5">
        <div className="min-w-0">
          <h1 className="text-[15px] font-semibold tracking-tight text-white">Resultados</h1>
          <p className="truncate text-[11px] text-zinc-500">
            {results.length === 0
              ? "Todavía no has escalado ninguna imagen"
              : `${results.length} ${results.length === 1 ? "imagen" : "imágenes"} · ${formatBytes(total)} en la biblioteca`}
          </p>
        </div>
        <span className="shrink-0 rounded-full bg-white/[0.06] px-2.5 py-1 text-[10px] font-medium text-zinc-400">
          SQLite
        </span>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-8 py-6">
        {results.length === 0 ? (
          <div className="flex h-full min-h-[320px] flex-col items-center justify-center rounded-2xl border border-dashed border-white/[0.09] bg-white/[0.012] px-8 py-12 text-center">
            <div className="grid h-14 w-14 place-items-center rounded-2xl border border-white/[0.08] bg-white/[0.03] text-zinc-400">
              <Icon d={ICONS.grid} className="h-6 w-6" />
            </div>
            <p className="mt-5 text-[15px] font-medium text-zinc-100">Sin resultados guardados</p>
            <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-zinc-500">
              Escala una tanda desde la pestaña «Escalar» y las imágenes aparecerán aquí, listas para
              copiar, descargar o eliminar.
            </p>
          </div>
        ) : (
          <div className="grid grid-cols-2 gap-3 xl:grid-cols-3 2xl:grid-cols-4">
            {results.map((result) => {
              const active = selected === result.id;
              return (
                <article
                  key={result.id}
                  onClick={() => onSelect(result.id)}
                  className={cx(
                    "group cursor-pointer overflow-hidden rounded-xl border bg-white/[0.018] transition",
                    active
                      ? "border-indigo-400/60 ring-1 ring-indigo-400/30"
                      : "border-white/[0.06] hover:border-white/[0.14]",
                  )}
                >
                  <div className="relative aspect-[4/3] bg-[#05060a]">
                    <img
                      src={resultSrc(result.id, true)}
                      alt={result.name}
                      loading="lazy"
                      draggable={false}
                      className="h-full w-full object-contain"
                    />
                    <span className="absolute left-2 top-2 rounded-full bg-black/65 px-2 py-0.5 text-[10px] text-zinc-200">
                      {levelLabel(result.level)} · {result.scale}×
                    </span>
                  </div>
                  <div className="space-y-2 p-2.5">
                    <p
                      className="truncate text-[12px] font-medium text-zinc-200"
                      title={result.name}
                    >
                      {result.name}
                    </p>
                    <p className="truncate text-[10px] text-zinc-500">
                      {result.width}×{result.height} · {formatBytes(result.size)} ·{" "}
                      {new Date(result.createdAt).toLocaleDateString("es-ES")}
                    </p>
                    <div className="flex items-center gap-1.5">
                      <button
                        type="button"
                        onClick={(event) => {
                          event.stopPropagation();
                          onCopy(result);
                        }}
                        className="flex-1 rounded-lg bg-white/[0.06] px-2 py-1.5 text-[11px] font-medium text-zinc-200 transition hover:bg-white/[0.12]"
                      >
                        Copiar
                      </button>
                      <button
                        type="button"
                        onClick={(event) => {
                          event.stopPropagation();
                          onDownload(result);
                        }}
                        className="flex-1 rounded-lg bg-white/[0.06] px-2 py-1.5 text-[11px] font-medium text-zinc-200 transition hover:bg-white/[0.12]"
                      >
                        Descargar
                      </button>
                      <button
                        type="button"
                        title="Eliminar de la biblioteca"
                        onClick={(event) => {
                          event.stopPropagation();
                          onDelete(result);
                        }}
                        className="grid h-7 w-7 shrink-0 place-items-center rounded-lg text-zinc-500 transition hover:bg-rose-500/10 hover:text-rose-300"
                      >
                        <Icon d={ICONS.x} className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  </div>
                </article>
              );
            })}
          </div>
        )}
      </div>
    </>
  );
}

function Preview({
  src,
  label,
  accent = false,
  onClick,
}: {
  src?: string;
  label: string;
  accent?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!onClick}
      title={onClick ? "Comparar antes y después" : label}
      className={cx(
        "relative h-12 w-16 shrink-0 overflow-hidden rounded-lg bg-black/40 ring-1 transition",
        accent ? "ring-indigo-400/30" : "ring-white/[0.08]",
        onClick ? "cursor-zoom-in hover:ring-indigo-400/70" : "cursor-default",
      )}
    >
      {src ? (
        <img
          src={src}
          alt={label}
          className="h-full w-full object-cover"
          draggable={false}
        />
      ) : (
        <div className="grid h-full w-full place-items-center text-zinc-600">
          <Icon d={ICONS.image} />
        </div>
      )}
      <span className="absolute inset-x-0 bottom-0 bg-black/55 py-[1px] text-center text-[8px] uppercase tracking-wide text-zinc-300">
        {label}
      </span>
    </button>
  );
}

function Comparator({
  item,
  outputSrc,
  originalSrc,
  onClose,
}: {
  item: Item;
  outputSrc: string;
  originalSrc: string;
  onClose: () => void;
}) {
  const [split, setSplit] = useState(50);
  const [zoom, setZoom] = useState<number | "fit">("fit");
  const stage = useRef<HTMLDivElement>(null);
  const outW = item.outWidth ?? item.width;
  const outH = item.outHeight ?? item.height;
  const fitted = zoom === "fit";

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const move = (clientX: number) => {
    const box = stage.current?.getBoundingClientRect();
    if (!box || box.width === 0) return;
    setSplit(Math.min(100, Math.max(0, ((clientX - box.left) / box.width) * 100)));
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/85 p-6 backdrop-blur-sm">
      <div className="flex h-full w-full max-w-6xl flex-col overflow-hidden rounded-2xl border border-white/10 bg-[#0b0d13] shadow-2xl">
        <header className="flex items-center justify-between gap-4 border-b border-white/[0.06] px-5 py-3">
          <div className="min-w-0">
            <p className="truncate text-[13px] font-medium text-zinc-100">{item.name}</p>
            <p className="text-[11px] text-zinc-500">
              {item.width}×{item.height} → {item.outWidth ?? item.width}×{item.outHeight ?? item.height}
              {item.ms ? ` · ${formatNumber(item.ms / 1000)} s` : ""}
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <div className="flex gap-1 rounded-lg border border-white/[0.06] bg-white/[0.02] p-1">
              <button
                type="button"
                onClick={() => setZoom("fit")}
                className={cx(
                  "rounded-md px-2 py-1 text-[11px] font-medium transition",
                  fitted ? "bg-white/[0.09] text-white" : "text-zinc-500 hover:text-zinc-300",
                )}
              >
                Ajustar
              </button>
              {[1, 2, 4].map((factor) => (
                <button
                  key={factor}
                  type="button"
                  onClick={() => setZoom(factor)}
                  className={cx(
                    "rounded-md px-2 py-1 text-[11px] font-medium transition",
                    zoom === factor
                      ? "bg-white/[0.09] text-white"
                      : "text-zinc-500 hover:text-zinc-300",
                  )}
                >
                  {factor * 100}%
                </button>
              ))}
            </div>
            <button
              type="button"
              onClick={onClose}
              className="grid h-8 w-8 place-items-center rounded-md text-zinc-400 transition hover:bg-white/[0.06] hover:text-white"
            >
              <Icon d={ICONS.x} />
            </button>
          </div>
        </header>

        <div className="relative min-h-0 flex-1">
          <div className="h-full overflow-auto bg-[#05060a]">
            <div
              ref={stage}
              onPointerDown={(event) => {
                event.currentTarget.setPointerCapture(event.pointerId);
                move(event.clientX);
              }}
              onPointerMove={(event) => {
                if (event.buttons === 1) move(event.clientX);
              }}
              className="relative mx-auto cursor-ew-resize touch-none select-none"
              style={
                fitted
                  ? { width: "100%", height: "100%" }
                  : { width: outW * zoom, height: outH * zoom }
              }
            >
              <img
                src={originalSrc}
                alt="Original"
                draggable={false}
                className={cx("absolute inset-0 h-full w-full", fitted && "object-contain")}
              />
              <div
                className="absolute inset-0"
                style={{ clipPath: `inset(0 0 0 ${split}%)` }}
              >
                <img
                  src={outputSrc}
                  alt="Resultado"
                  draggable={false}
                  className={cx("absolute inset-0 h-full w-full", fitted && "object-contain")}
                />
              </div>

              <div
                className="pointer-events-none absolute inset-y-0 w-px bg-white/80"
                style={{ left: `${split}%` }}
              >
                <div className="absolute left-1/2 top-1/2 grid h-8 w-8 -translate-x-1/2 -translate-y-1/2 place-items-center rounded-full bg-white text-[#0b0d13] shadow-lg">
                  <Icon d={ICONS.compare} className="h-4 w-4" />
                </div>
              </div>
            </div>
          </div>

          <span className="pointer-events-none absolute left-3 top-3 rounded-full bg-black/60 px-2 py-0.5 text-[10px] font-medium text-zinc-300">
            Original
          </span>
          <span className="pointer-events-none absolute right-3 top-3 rounded-full bg-indigo-500/80 px-2 py-0.5 text-[10px] font-medium text-white">
            Escalada
          </span>
        </div>

        <footer className="flex items-center justify-between gap-4 border-t border-white/[0.06] px-5 py-2.5 text-[11px] text-zinc-500">
          <span>Arrastra la línea y desplázate para comparar el detalle · Esc para cerrar</span>
          <span>
            {fitted ? "Ajustado a la ventana" : `${zoom * 100}% de los píxeles del resultado`}
          </span>
        </footer>
      </div>
    </div>
  );
}
