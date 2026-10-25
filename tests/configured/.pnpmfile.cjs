module.exports = {
  hooks: {
    readPackage(pkg) {
      if (pkg.name === 'is-number' && pkg.version === '7.0.0') {
        pkg.dependencies = { ...pkg.dependencies, 'is-odd': '^3.0.0' };
      }
      return pkg;
    },
  },
};
