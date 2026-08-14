// Compile-time Tailwind build — replaces the Play CDN script the templates
// used during development. Regenerate the sheet with `make css` after
// changing any template or the theme below (it must stay in sync with
// nothing: this file is now the only copy of the theme).
module.exports = {
  content: [
    "../templates/**/*.html",
    "../src/**/*.rs", // class names composed in Rust strings
  ],
  theme: {
    extend: {
      colors: {
        rust: { 950: '#0B0908', 900: '#14100D', 800: '#1E1712', 700: '#2B2018' },
        ember: { DEFAULT: '#FF6B24', soft: '#FF9A62', dim: '#B94716' },
        cream: '#F2EAE3',
        clay: '#A99282',
        copper: '#C47B45',
        sage: '#79C69A',
        blood: '#F2776F',
      },
      fontFamily: {
        display: ['"Bricolage Grotesque"', 'system-ui', 'sans-serif'],
        sans: ['"Bricolage Grotesque"', 'system-ui', 'sans-serif'],
        mono: ['"JetBrains Mono"', 'ui-monospace', 'monospace'],
      },
    },
  },
};
