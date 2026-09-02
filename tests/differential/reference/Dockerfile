FROM alpine:3.20@sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e AS build
COPY src.txt /src.txt
RUN cat /src.txt > /built.txt

FROM alpine:3.20@sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e
COPY --from=build /built.txt /io.txt
ENV APP=demo
WORKDIR /app
USER 1000
CMD ["/bin/echo", "hi"]