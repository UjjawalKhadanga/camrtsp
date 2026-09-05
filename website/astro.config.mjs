import { defineConfig } from "astro/config";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  site: "https://ujjawalkhadanga.github.io",
  base: process.env.ASTRO_BASE ?? "/",
  vite: {
    plugins: [tailwindcss()],
  },
});
