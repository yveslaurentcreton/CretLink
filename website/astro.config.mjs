// The CretLink documentation site (ST-M011-001): six short pages, published
// through GitHub Pages under https://yveslaurentcreton.github.io/CretLink/.

import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://yveslaurentcreton.github.io',
  base: '/CretLink',
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'CretLink',
      description: 'Attended remote support, driven by your own AI agent.',
      logo: { src: './src/assets/mark.svg' },
      favicon: '/favicon.svg',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/yveslaurentcreton/CretLink' },
      ],
      customCss: ['./src/styles/custom.css'],
      sidebar: [
        { label: 'What it is', link: '/' },
        { label: 'How a session works', slug: 'session' },
        { label: 'Get started', slug: 'get-started' },
        { label: 'Help someone', slug: 'help-someone' },
        { label: 'Host your own relay', slug: 'relay' },
        { label: 'Security in short', slug: 'security' },
      ],
    }),
  ],
});
