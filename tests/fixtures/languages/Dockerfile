ARG VERSION=1
FROM alpine AS base
ENV APP=codeq
FROM base AS build
RUN echo ok
FROM scratch AS final
COPY --from=build /app /app
