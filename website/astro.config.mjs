// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
	integrations: [
		starlight({
			title: 'glidesh v0.2.0',
			logo: {
				dark: './public/logo-white.webp',
				light: './public/logo-black.webp',
			},
			favicon: '/favicon.ico',
			customCss: ['./src/styles/custom.css'],
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/alessiopcc/glidesh' }],
			sidebar: [
				{ label: 'Home', slug: '' },
				{ label: 'Getting Started', slug: 'getting-started' },
				{
					label: 'Concepts',
					items: [
						{ label: 'Inventory', slug: 'concepts/inventory' },
						{ label: 'Plans', slug: 'concepts/plans' },
						{ label: 'Execution Modes', slug: 'concepts/execution-modes' },
						{ label: 'Variables', slug: 'concepts/variables' },
					],
				},
				{ label: 'CLI Reference', slug: 'cli' },
				{
					label: 'Modules',
					items: [
						{ label: 'Overview', slug: 'modules' },
						{ label: 'container', slug: 'modules/container' },
						{ label: 'disk', slug: 'modules/disk' },
						{ label: 'external', slug: 'modules/external' },
						{ label: 'file', slug: 'modules/file' },
						{ label: 'package', slug: 'modules/package' },
						{ label: 'shell', slug: 'modules/shell' },
						{ label: 'systemd', slug: 'modules/systemd' },
						{ label: 'user', slug: 'modules/user' },
					],
				},
				{
					label: 'Advanced',
					items: [
						{ label: 'Loops & Register', slug: 'advanced/loops-register' },
						{ label: 'Plan Includes', slug: 'advanced/plan-includes' },
						{ label: 'Writing Plugins', slug: 'advanced/writing-plugins' },
					],
				},
				{ label: 'Examples', slug: 'examples' },
			],
		}),
	],
});
