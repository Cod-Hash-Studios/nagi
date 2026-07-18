import { defineCollection } from 'astro:content';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
import { docsPath } from './docs-path';

export const collections = {
  docs: defineCollection({ loader: docsLoader({ generateId: docsPath }), schema: docsSchema() }),
};
