#!/usr/bin/env node
/**
 * Genera el `latest.json` que lee el actualizador de la app a partir del
 * instalador firmado de la versión actual y, con `--publish`, lo sube al
 * release de GitHub (creándolo si aún no existe).
 *
 *   npm run release:manifest          # sólo escribe latest.json
 *   npm run release:publish           # además lo sube/crea el release
 *
 * Opciones:
 *   --notes <archivo>    notas del release (Markdown). Por defecto, un título.
 *   --out <archivo>      dónde escribir latest.json (por defecto, junto al .exe).
 *   --repo <url>         repositorio si no se puede deducir del remoto git.
 *
 * El instalador y su firma deben existir antes:
 *   TAURI_SIGNING_PRIVATE_KEY=~/.tauri/upscaler.key npm run tauri build
 */

import { access, readFile, writeFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const has = (name) => args.includes(`--${name}`);
const value = (name) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 ? args[index + 1] : undefined;
};

const readJson = async (file) => JSON.parse(await readFile(file, "utf8"));

const config = await readJson(path.join(root, "src-tauri", "tauri.conf.json"));
const pkg = await readJson(path.join(root, "package.json"));
if (config.version !== pkg.version) {
  console.error(
    `Las versiones no coinciden: tauri.conf.json ${config.version} / package.json ${pkg.version}`,
  );
  process.exit(1);
}
const version = config.version;

const bundleDir = path.join(root, "src-tauri", "target", "release", "bundle", "nsis");
const installer = path.join(bundleDir, `Upscaler_${version}_x64-setup.exe`);
const signatureFile = `${installer}.sig`;
try {
  await access(installer);
  await access(signatureFile);
} catch {
  console.error(
    `Falta el instalador firmado de la ${version} en ${path.relative(root, bundleDir)}.\n` +
      "Compílalo antes: TAURI_SIGNING_PRIVATE_KEY=~/.tauri/upscaler.key npm run tauri build",
  );
  process.exit(1);
}
const signature = (await readFile(signatureFile, "utf8")).trim();

/** https://github.com/usuario/repo a partir del remoto, en ssh o https. */
function repository() {
  try {
    const remote = execFileSync("git", ["config", "--get", "remote.origin.url"], {
      cwd: root,
      encoding: "utf8",
    }).trim();
    const match = remote.match(/github\.com[:/]([^/]+)\/([^/]+?)(?:\.git)?$/);
    if (match) return `https://github.com/${match[1]}/${match[2]}`;
  } catch {
    // sin remoto: se pide con --repo
  }
  return undefined;
}

const repo = value("repo") ?? repository();
if (!repo) {
  console.error("No se pudo deducir el repositorio; pasa --repo https://github.com/usuario/repo");
  process.exit(1);
}

const assetName = path.basename(installer);
const url = `${repo.replace(/\/$/, "")}/releases/download/v${version}/${assetName}`;
const notesPath = value("notes");
const notes = notesPath
  ? (await readFile(path.resolve(notesPath), "utf8")).trim()
  : `## Upscaler ${version}`;

const latest = {
  version,
  notes,
  // el actualizador compara la fecha; sin zona rara, ISO en UTC
  pub_date: new Date().toISOString(),
  platforms: {
    // el bundle es NSIS para Windows x64
    "windows-x86_64": { signature, url },
  },
};
const outPath = path.resolve(value("out") ?? path.join(bundleDir, "latest.json"));
await writeFile(outPath, `${JSON.stringify(latest, null, 2)}\n`);
console.log(`latest.json → ${path.relative(root, outPath)} (v${version}, ${assetName})`);

if (!has("publish")) process.exit(0);

const tag = `v${version}`;
const releaseExists = () => {
  try {
    execFileSync("gh", ["release", "view", tag], { cwd: root, stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
};

if (releaseExists()) {
  execFileSync("gh", ["release", "upload", tag, outPath, "--clobber"], {
    cwd: root,
    stdio: "inherit",
  });
  console.log(`latest.json actualizado en el release ${tag}.`);
} else {
  const notesFile = notesPath ? path.resolve(notesPath) : path.join(bundleDir, "notas.md");
  if (!notesPath) await writeFile(notesFile, `${notes}\n`);
  execFileSync(
    "gh",
    [
      "release",
      "create",
      tag,
      installer,
      signatureFile,
      outPath,
      "--title",
      `Upscaler ${version}`,
      "--notes-file",
      notesFile,
    ],
    { cwd: root, stdio: "inherit" },
  );
  console.log(`Release ${tag} publicado con instalador, firma y latest.json.`);
}
