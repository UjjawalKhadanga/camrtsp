# camrtsp website

Static landing page for [camrtsp](https://github.com/UjjawalKhadanga/camrtsp).

## Stack

[Astro](https://astro.build) and [Tailwind CSS](https://tailwindcss.com). The build emits HTML, CSS, and a few bytes of vanilla JS for copy buttons and platform tabs. No React runtime.

That is the same shape as [Biome](https://github.com/biomejs/website) and [AstroWind](https://github.com/arthelokyo/astrowind): a static site you can put on GitHub Pages. It is lighter than Ghostty’s Next.js site or Frigate / Immich’s Docusaurus docs sites, which earn their weight once you have a large documentation tree.

## Develop

```bash
cd website
npm install
npm run dev
```

Open http://localhost:4321/

## Build

```bash
npm run build
npm run preview
```

GitHub Pages project deploys set `ASTRO_BASE=/camrtsp` so asset URLs resolve under `https://ujjawalkhadanga.github.io/camrtsp/`.
