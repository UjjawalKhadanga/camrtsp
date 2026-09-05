# camrtsp website

The product site for [camrtsp](https://github.com/UjjawalKhadanga/camrtsp), built with Astro, Tailwind CSS, and vanilla TypeScript. Static output; no client framework or animation dependency.

## Develop

```bash
cd website
npm ci
npm run dev
```

Open http://localhost:4321/.

## Build

```bash
npm run build
npm run preview
```

GitHub Pages project deploys use `ASTRO_BASE=/camrtsp`. The existing website workflow builds and deploys on pushes to main or master.

## Design and interactions

- [Brand assets](public/brand/): vector mark, README banner, raster social preview, and palette.
- CSS lens, orbital, scan, and wireframe animations respect `prefers-reduced-motion`.
- The simulated playground supports source selection, TCP/UDP selection, and pause/resume. It never requests camera access or opens a real stream.
- Platform setup tabs support arrow keys, Home, and End; copying always uses the selected platform’s commands.
- Clipboard failures show a manual-copy message instead of reporting success.

For changes, check desktop and mobile layouts, keyboard navigation, reduced motion, all playground controls, clipboard behavior, and the Pages base path.
