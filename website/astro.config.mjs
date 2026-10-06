// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import pkg from './package.json' with { type: 'json' };

export default defineConfig({
	integrations: [
		starlight({
			title: `glide·sh v${pkg.version}`,
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
						{ label: 'Idempotency & Drift', slug: 'concepts/idempotency' },
						{ label: 'Variables', slug: 'concepts/variables' },
						{ label: 'Secrets', slug: 'concepts/secrets' },
						{ label: 'TUI', slug: 'concepts/tui' },
						{ label: 'Logs', slug: 'concepts/logs' },
					],
				},
				{
					label: 'CLI Reference',
					items: [
						{ label: 'Overview', slug: 'cli' },
						{ label: 'Console', slug: 'cli/console' },
					],
				},
				{
					label: 'Modules',
					items: [
						{ label: 'Overview', slug: 'modules' },
						{ label: 'container', slug: 'modules/container' },
						{ label: 'disk', slug: 'modules/disk' },
						{ label: 'external', slug: 'modules/external' },
						{ label: 'file', slug: 'modules/file' },
						{ label: 'host', slug: 'modules/host' },
						{ label: 'nix', slug: 'modules/nix' },
						{ label: 'package', slug: 'modules/package' },
						{ label: 'shell', slug: 'modules/shell' },
						{ label: 'systemd', slug: 'modules/systemd' },
						{ label: 'user', slug: 'modules/user' },
					],
				},
				{
					label: 'Advanced',
					items: [
						{ label: 'Jump Hosts', slug: 'advanced/jump-hosts' },
						{ label: 'Privilege Escalation', slug: 'advanced/run-as' },
						{ label: 'Conditionals', slug: 'advanced/conditionals' },
						{ label: 'Loops & Register', slug: 'advanced/loops-register' },
						{ label: 'Subscribe', slug: 'advanced/subscribe' },
						{ label: 'Tags', slug: 'advanced/tags' },
						{ label: 'Waiting (until)', slug: 'advanced/until' },
						{ label: 'Rescue & Always', slug: 'advanced/rescue' },
						{ label: 'Plan Includes', slug: 'advanced/plan-includes' },
						{ label: 'Writing Plugins', slug: 'advanced/writing-plugins' },
					],
				},
				{ label: 'Examples', slug: 'examples' },
			],
		}),
	],
});
