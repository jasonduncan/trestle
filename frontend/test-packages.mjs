// Install packed artifacts into a fresh directory outside the workspace.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
const root = process.cwd(), target = mkdtempSync(join(tmpdir(), "trestle-consumer-"));
const run = (command, args, cwd = target) => execFileSync(command, args, { cwd, stdio: "pipe", encoding: "utf8" });
try {
  const dependencies = { react: "^19.3.0", "react-dom": "^19.3.0", "@types/react": "^19.3.0", "@types/react-dom": "^19.3.0" };
  for (const tool of ["vite", "typescript"]) dependencies[tool] = JSON.parse(readFileSync(resolve(root, "node_modules", tool, "package.json"), "utf8")).version;
  for (const name of ["client", "interaction-react", "admin-react"]) {
    const output = JSON.parse(run("npm", ["pack", "--json", "--pack-destination", target], resolve(root, name)))[0];
    dependencies["@trestle/" + name] = "file:" + join(target, output.filename);
    if (output.files.some((f) => f.path.startsWith("src/"))) throw new Error("Package contains source entry points");
  }
  writeFileSync(join(target, "package.json"), JSON.stringify({ name: "external-trestle-consumer", private: true, type: "module", dependencies }));
  run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund"]);
  writeFileSync(join(target, "index.html"), '<html><body><div id="root"></div><script type="module" src="/main.tsx"></script></body></html>');
  writeFileSync(join(target, "main.tsx"), 'import { createRoot } from "react-dom/client";\nimport { createTransport, OperationRuntime } from "@trestle/client";\nimport { OperationForm } from "@trestle/interaction-react";\nimport { AdminShell, WorkStatus } from "@trestle/admin-react";\nimport "@trestle/admin-react/styles.css";\nconst runtime = new OperationRuntime();\ncreateRoot(document.getElementById("root")!).render(<AdminShell transport={createTransport()} />);\nvoid [runtime, OperationForm, WorkStatus];\n');
  writeFileSync(join(target, "tsconfig.json"), JSON.stringify({ compilerOptions: { strict: true, noEmit: true, jsx: "react-jsx", target: "ES2022", module: "ESNext", moduleResolution: "bundler", skipLibCheck: false }, include: ["main.tsx"] }));
  run(process.execPath, [resolve(root, "node_modules/typescript/bin/tsc"), "-p", join(target, "tsconfig.json")]);
  run(process.execPath, [resolve(root, "node_modules/vite/bin/vite.js"), "build"]);
  for (const file of ["index.html", "main.tsx", "vite.config.ts"]) {
    const template = readFileSync(resolve(root, "../crates/trestle-cli/template-admin/frontend", file), "utf8");
    writeFileSync(join(target, file), template.replaceAll("{{name}}", "external-admin"));
  }
  run(process.execPath, [resolve(root, "node_modules/typescript/bin/tsc"), "-p", join(target, "tsconfig.json")]);
  run("npm", ["exec", "--offline", "--", "vite", "build"]);
  mkdirSync(resolve(root, "test-results"), { recursive: true });
  writeFileSync(resolve(root, "test-results/packages.json"), JSON.stringify({ result: "passed", packageNames: Object.keys(dependencies).filter((n) => n.startsWith("@trestle/")), checks: ["packed ESM", "external install", "public declarations", "CSS export", "production bundle", "scaffold frontend typecheck and production build"] }, null, 2));
  console.log("External package install, declarations and production build passed.");
} catch (error) {
  console.error(error); process.exitCode = 1;
} finally { rmSync(target, { recursive: true, force: true }); }
