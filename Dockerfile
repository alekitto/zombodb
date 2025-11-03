ARG PYTHON_VERSION=3.11

FROM public.ecr.aws/bitcompat/postgresql:15 as zombodb_build

USER root

RUN mkdir -p /app
RUN install_packages wget gnupg curl git gcc make build-essential libz-dev zlib1g-dev strace libssl-dev pkg-config build-essential ruby ruby-dev rubygems
RUN gem install --no-document fpm

ENV HOME=/root PATH="/root/.cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | bash -s -- -y

RUN cargo install cargo-pgrx --version 0.9.8 --locked
RUN cargo pgrx init --pg15=$(which pg_config)

ARG build=1
# RUN git clone https://github.com/alekitto/zombodb.git /app/zombodb
WORKDIR /app/zombodb
ADD . .

RUN install_packages libclang-dev
RUN cargo pgrx package
RUN find ./ -name "*.so" -exec strip {} \;

USER root:root
RUN mkdir /artifacts
RUN cd target/release/zombodb-pg15 && fpm -s dir -t deb -n zombodb-15 -v 3000.1.25 --deb-no-default-config-files -p /artifacts/zombodb_debian_pg15-3000.1.25_$(dpkg --print-architecture).deb -a $(dpkg --print-architecture) .

FROM public.ecr.aws/bitcompat/postgresql:15 as pgbm25_build

USER root

RUN mkdir -p /app
RUN install_packages wget gnupg curl git gcc make build-essential libz-dev zlib1g-dev strace libssl-dev pkg-config build-essential ruby ruby-dev rubygems libclang-dev
RUN gem install --no-document fpm

ENV HOME=/root PATH="/root/.cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | bash -s -- -y

RUN cargo install cargo-pgrx --version 0.11.1 --locked
RUN cargo pgrx init --pg15=$(which pg_config)

ARG build=1
RUN git clone https://github.com/paradedb/paradedb.git /app/paradedb
WORKDIR /app/paradedb/pg_bm25

RUN cargo pgrx package
RUN find ./ -name "*.so" -exec strip {} \;

USER root:root
RUN mkdir /artifacts
RUN cd ../target/release/pg_bm25-pg15 && fpm -s dir -t deb -n pg_bm25-pg15 -v 0.4.3 --deb-no-default-config-files -p /artifacts/pg_bm25_debian_pg15-0.4.3_$(dpkg --print-architecture).deb -a $(dpkg --print-architecture) .

FROM public.ecr.aws/bitcompat/python:${PYTHON_VERSION} as python
FROM public.ecr.aws/bitcompat/postgresql:15 as runtime

USER root
ENV PATH="/opt/bitnami/node/bin:/opt/bitnami/python/bin:$PATH" \
    LD_LIBRARY_PATH=/opt/bitnami/python/lib/

COPY --from=python /opt/bitnami/python /opt/bitnami/python
COPY --from=zombodb_build /artifacts /artifacts
COPY --from=pgbm25_build /artifacts /artifacts

ARG PG_BASEDIR=/opt/bitnami/postgresql
RUN <<EOT bash
    set -ex

    install_packages bzip2 gcc g++ pkg-config libicu-dev flex bison libreadline-dev \
        zlib1g-dev libldap2-dev libpam-dev libssl-dev libxml2-dev libxml2-utils libxslt1-dev libzstd-dev \
        uuid-dev gettext libperl-dev libipc-run-perl liblz4-dev xsltproc zstd git libpcre3-dev \
        make lsb-release autoconf automake libtool libcurl4-gnutls-dev

    export PG_MAJOR=\$(echo "${APP_VERSION}" | cut -d'.' -f1)
    export PG_MINOR=\$(echo "${APP_VERSION}" | cut -d'.' -f2)

    mkdir -p /opt/src
    cd /opt/src
    curl -sSL -opostgresql.tar.bz2 https://ftp.postgresql.org/pub/source/v\$PG_MAJOR.\$PG_MINOR/postgresql-\$PG_MAJOR.\$PG_MINOR.tar.bz2
    tar xf postgresql.tar.bz2

    cd postgresql-\$PG_MAJOR.\$PG_MINOR
    ./configure --with-python \
      --prefix=$PG_BASEDIR \
      --sysconfdir=$PG_BASEDIR/etc \
      --datarootdir=$PG_BASEDIR/share \
      --datadir=$PG_BASEDIR/share \
      --bindir=$PG_BASEDIR/bin \
      --libdir=$PG_BASEDIR/lib/ \
      --libexecdir=$PG_BASEDIR/lib/postgresql/ \
      --includedir=$PG_BASEDIR/include/

    cd src/pl/plpython
    make -j$(nproc)
    make install

    cd /
    rm -rf /opt/src

    git clone -b 2.4.2 --depth 1 https://github.com/RhodiumToad/ip4r.git /usr/local/src/ip4r
    cd /usr/local/src/ip4r
    make install

    git clone https://github.com/gavinwahl/postgres-json-schema.git /usr/local/src/postgres-json-schema
    cd /usr/local/src/postgres-json-schema
    make install

    git clone git://sigaev.ru/smlar.git /usr/local/src/smlar
    cd /usr/local/src/smlar
    make USE_PGXS=1 && make USE_PGXS=1 install

    apt-get install -y /artifacts/*.deb
    rm -rf /usr/local/src/* /artifacts

    apt-get purge -y bzip2 gcc g++ \
        pkg-config libicu-dev flex bison libreadline-dev \
        zlib1g-dev libldap2-dev libpam-dev libssl-dev libxml2-dev libxml2-utils libxslt1-dev libzstd-dev \
        uuid-dev gettext libperl-dev libipc-run-perl liblz4-dev xsltproc zstd git \
        libpcre3-dev make lsb-release autoconf automake libtool libcurl4-gnutls-dev && apt-get autoremove -y
    rm -rf /var/lib/apt/lists/*
EOT

USER 1001
