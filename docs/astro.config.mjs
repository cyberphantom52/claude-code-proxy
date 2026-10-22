// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLlmsTxt from 'starlight-llms-txt';

export default defineConfig({
  site: 'https://claude-code-proxy.raine.dev',
  redirects: {
    '/using/switching-models-and-backends/': '/using/switching-models/',
    '/providers/choosing-a-provider/': '/providers/codex/',
  },
  integrations: [
    starlight({
      title: 'claude-code-proxy',
      description: 'Run Claude Code with Codex through your ChatGPT subscription.',
      plugins: [starlightLlmsTxt()],
      favicon: '/favicon.svg',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/raine/claude-code-proxy',
        },
      ],
      components: {
        SocialIcons: './src/components/HeaderLinks.astro',
      },
      customCss: ['./src/styles/site.css'],
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'What is claude-code-proxy?', link: '/' },
            { label: 'Getting started', slug: 'getting-started' },
            { label: 'How it works', slug: 'how-it-works' },
          ],
        },
        {
          label: 'Codex backend',
          items: [
            { label: 'Codex and ChatGPT login', slug: 'providers/codex' },
            { label: 'Source build and integration', slug: 'using/chatgpt-integration' },
          ],
        },
        {
          label: 'Using the proxy',
          items: [
            { label: 'Configure Claude Code', slug: 'using/configure-claude-code' },
            { label: 'Models and routing', slug: 'using/models-and-routing' },
            { label: 'Switching models', slug: 'using/switching-models' },
            { label: 'Monitor TUI', slug: 'using/monitor-tui' },
            { label: 'For coding agents', slug: 'using/for-coding-agents' },
            { label: 'Troubleshooting', slug: 'using/troubleshooting' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Command reference', slug: 'reference/command-reference' },
            { label: 'Configuration', slug: 'reference/configuration' },
            { label: 'Files and storage', slug: 'reference/files-and-storage' },
            { label: 'HTTP API', slug: 'reference/http-api' },
            { label: 'Compatibility and limitations', slug: 'reference/compatibility-and-limitations' },
            { label: 'Changelog', slug: 'reference/changelog' },
          ],
        },
      ],
    }),
  ],
});
