# Build with the logistics-frontend checkout as the context (kind.sh does).
# The nginx config is mounted from a ConfigMap (nginx.conf), not baked in.
FROM node:22-bookworm-slim AS builder
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
# VITE_API_BASE_URL stays unset, so the app calls /api on its own origin.
RUN npm run build

FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=builder /app/dist /usr/share/nginx/html
EXPOSE 8080
